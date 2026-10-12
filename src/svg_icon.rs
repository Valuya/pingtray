//! Tray icons as SVG: GNOME renders them at the screen's real resolution
//! (crisp at 150 % or 200 % scaling, where a PNG gets blown up and blurry),
//! with real fonts and smooth shapes.

use std::fmt::Write;

use crate::colors;
use crate::icon::{Column, Current, IconData, Thresholds, ceiling_ms};

const FONT: &str = "Cantarell, Ubuntu, 'DejaVu Sans', sans-serif";
const WHITE: &str = "#f6f5f4";

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Current median as a number, then the recent history SmokePing style.
pub fn ping(data: &IconData, height: f64, th: Thresholds) -> String {
    let h = height;
    let text_w = h * 1.35;
    let gap = h * 0.2;
    let graph_w = (h * 2.6).max(data.columns.len() as f64 * 1.5);
    let w = text_w + gap + graph_w;
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.2} {h:.2}">"#
    );
    s.push_str(r#"<defs><filter id="soft" x="-5%" y="-20%" width="110%" height="140%"><feGaussianBlur stdDeviation="0.45"/></filter></defs>"#);

    // the number
    let (label, color) = match data.current {
        Current::Unknown => ("?".to_string(), "#9a9996".to_string()),
        Current::Lost => ("–".to_string(), hex(colors::BAD)),
        Current::Ms(ms) => {
            let label = match ms {
                m if m < 9.95 => format!("{m:.1}"),
                m if m < 999.5 => format!("{m:.0}"),
                m => format!("{:.1}s", m / 1000.0),
            };
            let color = if colors::latency_level(ms, th) == 0 { WHITE.to_string() } else { hex(colors::latency_rgb(ms, th)) };
            (label, color)
        }
    };
    let size = if label.chars().count() > 3 { h * 0.55 } else { h * 0.72 };
    let _ = write!(
        s,
        r#"<text x="{:.2}" y="{:.2}" text-anchor="end" font-family="{FONT}" font-weight="700" font-size="{size:.2}" fill="{color}">{label}</text>"#,
        text_w,
        h * 0.5 + size * 0.36
    );

    // the graph: frame, smoke, median
    let x0 = text_w + gap;
    let (top, bottom) = (0.5, h - 0.5);
    let ceiling = ceiling_ms(&data.columns);
    let y = |ms: f64| bottom - (ms / ceiling).clamp(0.0, 1.0) * (bottom - top);
    let _ = write!(
        s,
        r#"<rect x="{x0:.2}" y="0" width="{graph_w:.2}" height="{h:.2}" rx="{:.2}" fill="{WHITE}" fill-opacity="0.09"/>"#,
        h * 0.12
    );
    let _ = write!(
        s,
        r#"<text x="{:.2}" y="{:.2}" font-family="{FONT}" font-size="{:.2}" fill="{WHITE}" fill-opacity="0.45">{ceiling:.0}</text>"#,
        x0 + 1.0,
        h * 0.3,
        h * 0.27
    );
    let n = data.columns.len().max(1);
    let cw = graph_w / n as f64;
    let cx = |i: usize| x0 + (i as f64 + 0.5) * cw;

    // runs of columns with replies
    let mut runs: Vec<Vec<(usize, [f64; 5], f64)>> = Vec::new();
    for (i, c) in data.columns.iter().enumerate() {
        match c {
            Column { pcts: Some(p), loss, .. } => match runs.last_mut() {
                Some(r) if r.last().is_some_and(|l| l.0 + 1 == i) => r.push((i, *p, *loss)),
                _ => runs.push(vec![(i, *p, *loss)]),
            },
            Column { count, .. } if *count > 0 => {
                // nothing came back at all
                let _ = write!(
                    s,
                    r#"<rect x="{:.2}" y="0" width="{cw:.2}" height="{h:.2}" fill="{}" fill-opacity="0.85"/>"#,
                    x0 + i as f64 * cw,
                    hex(colors::OUTAGE)
                );
            }
            _ => {}
        }
    }
    for run in &runs {
        // a lone column still gets some width
        let xs: Vec<f64> = if run.len() == 1 {
            vec![cx(run[0].0) - cw / 2.0, cx(run[0].0) + cw / 2.0]
        } else {
            run.iter().map(|c| cx(c.0)).collect()
        };
        let vals = |k: usize| -> Vec<f64> {
            if run.len() == 1 { vec![run[0].1[k]; 2] } else { run.iter().map(|c| c.1[k]).collect() }
        };
        for (lo, hi, opacity) in [(0, 4, 0.22), (1, 3, 0.38)] {
            let (los, his) = (vals(lo), vals(hi));
            let mut d = String::new();
            for (i, x) in xs.iter().enumerate() {
                let _ = write!(d, "{}{:.2},{:.2} ", if i == 0 { "M" } else { "L" }, x, y(his[i]));
            }
            for (i, x) in xs.iter().enumerate().rev() {
                let _ = write!(d, "L{:.2},{:.2} ", x, y(los[i]));
            }
            let _ = write!(s, r#"<path d="{d}Z" fill="{WHITE}" fill-opacity="{opacity}" filter="url(#soft)"/>"#);
        }
        let med = vals(2);
        let losses: Vec<f64> = if run.len() == 1 { vec![run[0].2; 2] } else { run.iter().map(|c| c.2).collect() };
        for i in 0..xs.len().saturating_sub(1) {
            let _ = write!(
                s,
                r#"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="{}" stroke-width="{:.2}" stroke-linecap="round"/>"#,
                xs[i],
                y(med[i]),
                xs[i + 1],
                y(med[i + 1]),
                hex(colors::loss_rgb(losses[i].max(losses[i + 1]))),
                (h * 0.09).max(1.2)
            );
        }
    }
    s.push_str("</svg>");
    s
}

/// Battery with its charge, a bolt while charging, and the mode as a
/// symbol: arrow up to the top for Maximize, a heart for Preserve (with a
/// tick where charging stops).
pub fn battery(percentage: f64, preserve: bool, threshold: u32, charging: bool, height: f64) -> String {
    let h = height;
    let body_h = h * 0.62;
    let body_w = h * 1.25;
    let (bx, by) = (0.75, (h - body_h) / 2.0);
    let nub_w = h * 0.1;
    let sym = h * 0.72;
    let gap = h * 0.18;
    let w = bx + body_w + nub_w + gap + sym + 0.5;
    let stroke = (h * 0.075).max(1.1);
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.0}" height="{h:.0}" viewBox="0 0 {w:.2} {h:.2}">"#
    );
    // outline and nub
    let _ = write!(
        s,
        r#"<rect x="{bx:.2}" y="{by:.2}" width="{body_w:.2}" height="{body_h:.2}" rx="{:.2}" fill="none" stroke="{WHITE}" stroke-width="{stroke:.2}"/>"#,
        h * 0.12
    );
    let _ = write!(
        s,
        r#"<rect x="{:.2}" y="{:.2}" width="{nub_w:.2}" height="{:.2}" rx="{:.2}" fill="{WHITE}"/>"#,
        bx + body_w + stroke / 2.0,
        by + body_h * 0.3,
        body_h * 0.4,
        nub_w / 2.0
    );
    // charge level
    let pad = stroke * 1.6;
    let inner_w = body_w - 2.0 * pad;
    let fill_w = inner_w * (percentage.clamp(0.0, 100.0) / 100.0);
    let _ = write!(
        s,
        r#"<rect x="{:.2}" y="{:.2}" width="{fill_w:.2}" height="{:.2}" rx="{:.2}" fill="{WHITE}" fill-opacity="{}"/>"#,
        bx + pad,
        by + pad,
        body_h - 2.0 * pad,
        h * 0.05,
        if charging { 0.95 } else { 0.6 }
    );
    if preserve {
        let x = bx + pad + inner_w * threshold.min(100) as f64 / 100.0;
        let _ = write!(
            s,
            r#"<line x1="{x:.2}" y1="{:.2}" x2="{x:.2}" y2="{:.2}" stroke="{WHITE}" stroke-width="{:.2}"/>"#,
            by - h * 0.08,
            by + body_h + h * 0.08,
            stroke * 0.9
        );
    }
    if charging {
        // bolt, cut out of the fill
        let (cx, cy, k) = (bx + body_w / 2.0, h / 2.0, body_h * 0.075);
        let pts = [(0.5, -5.0), (-2.5, 0.6), (-0.2, 0.6), (-1.0, 5.0), (2.5, -0.9), (0.2, -0.9)];
        let d: Vec<String> = pts.iter().map(|(x, y)| format!("{:.2},{:.2}", cx + x * k, cy + y * k)).collect();
        let _ = write!(
            s,
            r##"<polygon points="{}" fill="#241f31" stroke="{WHITE}" stroke-width="{:.2}" stroke-linejoin="round"/>"##,
            d.join(" "),
            stroke * 0.5
        );
    }
    // the mode
    let (sx, sy, u) = (bx + body_w + nub_w + gap, (h - sym) / 2.0, sym / 10.0);
    let p = |x: f64, y: f64| format!("{:.2},{:.2}", sx + x * u, sy + y * u);
    if preserve {
        let _ = write!(
            s,
            r#"<path d="M{} C{} {} {} C{} {} {} C{} {} {} C{} {} {} Z" fill="{WHITE}"/>"#,
            p(5.0, 9.2),
            p(1.0, 6.4),
            p(-0.2, 4.0),
            p(1.4, 2.1),
            p(2.8, 0.6),
            p(4.5, 1.1),
            p(5.0, 2.7),
            p(5.5, 1.1),
            p(7.2, 0.6),
            p(8.6, 2.1),
            p(10.2, 4.0),
            p(9.0, 6.4),
            p(5.0, 9.2)
        );
    } else {
        let _ = write!(
            s,
            r#"<rect x="{}" y="{}" width="{:.2}" height="{:.2}" rx="{:.2}" fill="{WHITE}"/>"#,
            format!("{:.2}", sx + 0.8 * u),
            format!("{:.2}", sy + 0.3 * u),
            8.4 * u,
            1.5 * u,
            0.4 * u
        );
        let _ = write!(
            s,
            r#"<polygon points="{} {} {} {} {} {} {}" fill="{WHITE}"/>"#,
            p(5.0, 2.6),
            p(9.2, 6.6),
            p(6.5, 6.6),
            p(6.5, 9.8),
            p(3.5, 9.8),
            p(3.5, 6.6),
            p(0.8, 6.6)
        );
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_wide_svgs() {
        let data = IconData { current: Current::Ms(28.0), columns: vec![Column::from_samples(&[Some(25.0), Some(31.0)]); 20] };
        let th = Thresholds { warn_ms: 60.0, bad_ms: 200.0 };
        let svg = ping(&data, 20.0, th);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>") && svg.contains(">28<"));
        let b = battery(62.0, true, 80, true, 20.0);
        assert!(b.contains("<path") && b.contains("<polygon"));
    }
}
