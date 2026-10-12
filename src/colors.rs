//! Every colour that carries meaning, in one place, so that meanings never
//! share a hue:
//! - connection quality (latency and loss) is one green → red ramp, used
//!   everywhere quality is shown (tray, loss marks, map);
//! - destinations get hues outside that ramp (blues, pinks, greys);
//! - transport modes are coloured on the map only, where they replace quality.

use crate::icon::Thresholds;

/// Quality ramp, from good to no connection at all.
pub const GOOD: [u8; 3] = [0x2e, 0xc2, 0x7e];
pub const DEGRADED: [u8; 3] = [0xf6, 0xd3, 0x2d];
pub const POOR: [u8; 3] = [0xff, 0x78, 0x00];
pub const BAD: [u8; 3] = [0xe0, 0x1b, 0x24];
pub const OUTAGE: [u8; 3] = [0x8b, 0x10, 0x1a];
const RAMP: [[u8; 3]; 5] = [GOOD, DEGRADED, POOR, BAD, OUTAGE];

/// What each step of the ramp means, for legends.
pub const RAMP_LEGEND: [(&str, [u8; 3]); 5] = [
    ("good", GOOD),
    ("degraded: slow, or loss < 5 %", DEGRADED),
    ("poor: very slow, or loss < 20 %", POOR),
    ("bad: loss ≥ 20 %", BAD),
    ("no connection", OUTAGE),
];

pub const LOSS_LEGEND: [(&str, [u8; 3]); 4] =
    [("< 5 %", DEGRADED), ("< 20 %", POOR), ("< 100 %", BAD), ("all lost", OUTAGE)];

/// Step of the ramp for a lost fraction (0..=1).
pub fn loss_level(loss: f64) -> usize {
    match loss {
        l if l <= 0.0 => 0,
        l if l < 0.05 => 1,
        l if l < 0.20 => 2,
        l if l < 1.0 => 3,
        _ => 4,
    }
}

/// Step of the ramp for a reply time; latency alone never goes past "poor".
pub fn latency_level(ms: f64, th: Thresholds) -> usize {
    if ms >= th.bad_ms {
        2
    } else if ms >= th.warn_ms {
        1
    } else {
        0
    }
}

pub fn loss_rgb(loss: f64) -> [u8; 3] {
    RAMP[loss_level(loss)]
}

pub fn latency_rgb(ms: f64, th: Thresholds) -> [u8; 3] {
    RAMP[latency_level(ms, th)]
}

/// Worst of loss and latency.
pub fn quality_rgb(loss: f64, median_ms: Option<f64>, th: Thresholds) -> [u8; 3] {
    let lat = median_ms.map_or(0, |m| latency_level(m, th));
    RAMP[loss_level(loss).max(lat)]
}

/// Destination colours: none of them on the quality ramp.
pub const TARGETS: [[u8; 3]; 6] = [
    [0x62, 0xa0, 0xea], // blue
    [0xdc, 0x8a, 0xdd], // pink
    [0x33, 0xc7, 0xde], // cyan
    [0xb5, 0xa3, 0xff], // lavender
    [0xde, 0xdd, 0xda], // light grey
    [0x1c, 0x71, 0xd8], // deep blue
];
