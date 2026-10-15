//! "Every day around 8:30": the pings of every day's window around one time
//! of day, placed where they were sent from, merged into small map cells.
//! Shows the usual connection along a regular journey at that hour.

use std::collections::{BTreeSet, HashMap};

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveTime, TimeZone, Weekday};

use crate::location::estimate_at;
use crate::store::Store;
use crate::transport::{self, Mode};

/// Cell size: about 220 m north-south, 210 m east-west in Belgium.
pub const CELL_LAT: f64 = 0.002;
pub const CELL_LON: f64 = 0.003;
/// Pings placed less precisely than this are left out.
const MAX_UNCERTAINTY_M: f64 = 500.0;

/// Transport modes as the filter groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Still,
    Foot,
    Bike,
    Road,
    Rail,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Still, Group::Foot, Group::Bike, Group::Road, Group::Rail];

    pub fn of(mode: Mode) -> Group {
        match mode {
            Mode::Still => Group::Still,
            Mode::Foot => Group::Foot,
            Mode::Bike => Group::Bike,
            Mode::Car | Mode::Vehicle => Group::Road,
            Mode::Train | Mode::Tram | Mode::Metro => Group::Rail,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Group::Still => "still",
            Group::Foot => "on foot",
            Group::Bike => "bike",
            Group::Road => "car / bus",
            Group::Rail => "train / tram",
        }
    }
}

/// Which pings to keep: all modes, or only these.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeFilter(pub Option<BTreeSet<Group>>);

impl ModeFilter {
    pub fn allows(&self, mode: Option<Mode>) -> bool {
        match (&self.0, mode) {
            (None, _) => true,
            (Some(set), Some(m)) => set.contains(&Group::of(m)),
            // unknown mode: only when nothing is filtered out
            (Some(_), None) => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub target_id: i64,
    /// Minutes after midnight, local time.
    pub minute_of_day: u32,
    /// Minutes before and after.
    pub half_window: u32,
    pub days: u32,
    pub weekdays_only: bool,
    pub modes: ModeFilter,
    pub max_gap_ms: i64,
}

#[derive(Clone, Debug)]
pub struct Cell {
    pub lat: f64,
    pub lon: f64,
    pub count: u32,
    pub lost: u32,
    pub median_ms: Option<f64>,
    pub days: u32,
}

impl Cell {
    pub fn loss(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.lost as f64 / self.count as f64 }
    }
}

pub struct Result {
    pub params: Params,
    pub cells: Vec<Cell>,
    pub days_with_data: u32,
}

pub fn compute(store: &Store, p: &Params) -> Result {
    let today = Local::now().date_naive();
    let time = NaiveTime::from_num_seconds_from_midnight_opt(p.minute_of_day.min(1439) * 60, 0).unwrap_or_default();
    let half = p.half_window as i64 * 60_000;
    #[derive(Default)]
    struct Acc {
        count: u32,
        lost: u32,
        rtts: Vec<f64>,
        days: BTreeSet<i64>,
    }
    let mut cells: HashMap<(i64, i64), Acc> = HashMap::new();
    let mut days_with_data = 0;
    for back in 0..p.days as i64 {
        let date = today - ChronoDuration::days(back);
        if p.weekdays_only && matches!(date.weekday(), Weekday::Sat | Weekday::Sun) {
            continue;
        }
        let Some(centre) = Local.from_local_datetime(&date.and_time(time)).earliest() else { continue };
        let centre = centre.timestamp_millis();
        let (from, to) = (centre - half, centre + half);
        let Ok(fixes) = store.fixes(from - p.max_gap_ms, to + p.max_gap_ms) else { continue };
        if fixes.is_empty() {
            continue;
        }
        let Ok(samples) = store.samples_between(p.target_id, from, to) else { continue };
        let segments = if p.modes.0.is_some() {
            transport::classify(&fixes, p.max_gap_ms, |t| transport::rails_cached(store, t))
        } else {
            Vec::new()
        };
        let mut any = false;
        for (ts, rtt) in samples {
            if p.modes.0.is_some() {
                let mode = segments.iter().find(|s| s.from_ms <= ts && ts <= s.to_ms).map(|s| s.mode);
                if !p.modes.allows(mode) {
                    continue;
                }
            }
            let Some(e) = estimate_at(&fixes, ts, p.max_gap_ms) else { continue };
            if e.uncertainty_m > MAX_UNCERTAINTY_M {
                continue;
            }
            let key = ((e.lat / CELL_LAT).floor() as i64, (e.lon / CELL_LON).floor() as i64);
            let acc = cells.entry(key).or_default();
            acc.count += 1;
            match rtt {
                Some(ms) => acc.rtts.push(ms),
                None => acc.lost += 1,
            }
            acc.days.insert(back);
            any = true;
        }
        if any {
            days_with_data += 1;
        }
    }
    let cells = cells
        .into_iter()
        .map(|((y, x), mut a)| {
            a.rtts.sort_by(f64::total_cmp);
            Cell {
                lat: y as f64 * CELL_LAT,
                lon: x as f64 * CELL_LON,
                count: a.count,
                lost: a.lost,
                median_ms: (!a.rtts.is_empty()).then(|| crate::icon::percentile(&a.rtts, 0.5)),
                days: a.days.len() as u32,
            }
        })
        .collect();
    Result { params: p.clone(), cells, days_with_data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_groups() {
        let rail = ModeFilter(Some([Group::Rail].into_iter().collect()));
        assert!(rail.allows(Some(Mode::Train)) && rail.allows(Some(Mode::Tram)));
        assert!(!rail.allows(Some(Mode::Car)) && !rail.allows(None));
        assert!(ModeFilter(None).allows(None));
    }
}
