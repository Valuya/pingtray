//! Renders the tray icon: the current reply time as a number, followed by a
//! SmokePing-style mini graph (grey "smoke" for the spread of the pings in each
//! column, a median tick coloured by packet loss).

use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub struct Thresholds {
    pub warn_ms: f64,
    pub bad_ms: f64,
}

/// Pings of all targets pooled over one column's time slice.
#[derive(Clone, Debug, Default)]
pub struct Column {
    /// min, p25, median, p75, max in ms; `None` when nothing came back.
    pub pcts: Option<[f64; 5]>,
    /// Lost fraction, 0..=1.
    pub loss: f64,
    pub count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Current {
    Unknown,
    Lost,
    Ms(f64),
}

#[derive(Clone, Debug)]
pub struct IconData {
    pub current: Current,
    pub columns: Vec<Column>,
}

impl Column {
    pub fn from_samples(samples: &[Option<f64>]) -> Column {
        let mut ok: Vec<f64> = samples.iter().flatten().copied().collect();
        ok.sort_by(f64::total_cmp);
        let count = samples.len();
        let loss = if count == 0 { 0.0 } else { (count - ok.len()) as f64 / count as f64 };
        let pcts = (!ok.is_empty()).then(|| [0.0, 0.25, 0.5, 0.75, 1.0].map(|p| percentile(&ok, p)));
        Column { pcts, loss, count }
    }
}

/// Linear-interpolated percentile of sorted, non-empty data.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let pos = p * (sorted.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

/// Upper bound of the graph: a round value above typical columns. Outliers
/// are clipped at the top rather than squashing everything else.
pub fn ceiling_ms(columns: &[Column]) -> f64 {
    let mut p75: Vec<f64> = columns.iter().filter_map(|c| c.pcts.map(|p| p[3])).collect();
    p75.sort_by(f64::total_cmp);
    let top = if p75.is_empty() { 0.0 } else { percentile(&p75, 0.8) * 1.5 };
    [20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0]
        .into_iter()
        .find(|&c| c >= top)
        .unwrap_or(5000.0)
}

// 5x7 bitmap glyphs, one u8 per row (low 5 bits, MSB = left).
const GLYPH_W: usize = 5;
const GLYPH_H: usize = 7;
fn glyph(c: char) -> [u8; GLYPH_H] {
    match c {
        '0' => [0x0e, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0e],
        '1' => [0x04, 0x0c, 0x04, 0x04, 0x04, 0x04, 0x0e],
        '2' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1f],
        '3' => [0x1f, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0e],
        '4' => [0x02, 0x06, 0x0a, 0x12, 0x1f, 0x02, 0x02],
        '5' => [0x1f, 0x10, 0x1e, 0x01, 0x01, 0x11, 0x0e],
        '6' => [0x06, 0x08, 0x10, 0x1e, 0x11, 0x11, 0x0e],
        '7' => [0x1f, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0e, 0x11, 0x11, 0x0e, 0x11, 0x11, 0x0e],
        '9' => [0x0e, 0x11, 0x11, 0x0f, 0x01, 0x02, 0x0c],
        '-' => [0x00, 0x00, 0x00, 0x1f, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c],
        'k' => [0x10, 0x10, 0x12, 0x14, 0x18, 0x14, 0x12],
        '?' => [0x0e, 0x11, 0x01, 0x02, 0x04, 0x00, 0x04],
        'M' => [0x11, 0x1b, 0x15, 0x15, 0x11, 0x11, 0x11],
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'X' => [0x11, 0x11, 0x0a, 0x04, 0x0a, 0x11, 0x11],
        '%' => [0x19, 0x1a, 0x02, 0x04, 0x08, 0x0b, 0x13],
        _ => [0; GLYPH_H],
    }
}

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, px: vec![0; w * h * 4] }
    }

    /// Alpha-blends `rgb` at opacity `a` over the pixel.
    fn blend(&mut self, x: usize, y: usize, rgb: [u8; 3], a: u8) {
        if x >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + x) * 4;
        let (sa, da) = (a as u32, self.px[i + 3] as u32);
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            return;
        }
        for (k, &c) in rgb.iter().enumerate() {
            let s = c as u32 * sa;
            let d = self.px[i + k] as u32 * da * (255 - sa) / 255;
            self.px[i + k] = ((s + d) / out_a) as u8;
        }
        self.px[i + 3] = out_a as u8;
    }

    fn rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, rgb: [u8; 3], a: u8) {
        for y in y0..y1.min(self.h) {
            for x in x0..x1.min(self.w) {
                self.blend(x, y, rgb, a);
            }
        }
    }

    fn text(&mut self, s: &str, x_right: usize, y0: usize, scale: usize, rgb: [u8; 3]) {
        let adv = (GLYPH_W + 1) * scale;
        let mut x = x_right.saturating_sub(s.chars().count() * adv - scale);
        for c in s.chars() {
            for (row, bits) in glyph(c).iter().enumerate() {
                for col in 0..GLYPH_W {
                    if bits & (0x10 >> col) != 0 {
                        let (px, py) = (x + col * scale, y0 + row * scale);
                        self.rect(px, py, px + scale, py + scale, rgb, 0xff);
                    }
                }
            }
            x += adv;
        }
    }
}

const TEXT_CHARS: usize = 3;
const COLUMN_W: usize = 2;

/// Renders the icon at `height` pixels; returns (width, height, RGBA).
pub fn render(data: &IconData, height: usize, th: Thresholds) -> (usize, usize, Vec<u8>) {
    let h = height.max(GLYPH_H + 2);
    let scale = ((h - 2) / GLYPH_H).max(1);
    let text_w = TEXT_CHARS * (GLYPH_W + 1) * scale - scale;
    let gap = scale * 2;
    let graph_x = text_w + gap;
    let graph_w = data.columns.len() * COLUMN_W;
    let mut c = Canvas::new(graph_x + graph_w, h);

    // current value
    let (label, rgb) = match data.current {
        Current::Unknown => ("?".to_string(), [0x9a, 0x99, 0x96]),
        Current::Lost => ("--".to_string(), crate::colors::BAD),
        Current::Ms(ms) => {
            let label = match ms {
                m if m < 9.95 => format!("{m:.1}"),
                m if m < 999.5 => format!("{m:.0}"),
                m => format!("{:.0}k", (m / 1000.0).min(99.0)),
            };
            // plain white when fine, the quality ramp otherwise
            let rgb = match crate::colors::latency_level(ms, th) {
                0 => [0xf6, 0xf5, 0xf4],
                _ => crate::colors::latency_rgb(ms, th),
            };
            (label, rgb)
        }
    };
    let text_y = (h - GLYPH_H * scale) / 2;
    c.text(&label, text_w, text_y, scale, rgb);

    // graph frame, so that bar heights read against a fixed top
    let white = [0xff, 0xff, 0xff];
    c.rect(graph_x, 0, graph_x + graph_w, h, white, 0x1c);
    let ceiling = ceiling_ms(&data.columns);
    let y_of = |ms: f64| -> usize {
        let frac = (ms / ceiling).clamp(0.0, 1.0);
        ((1.0 - frac) * (h - 1) as f64).round() as usize
    };
    for (i, col) in data.columns.iter().enumerate() {
        let x0 = graph_x + i * COLUMN_W;
        let x1 = x0 + COLUMN_W;
        if col.count == 0 {
            continue;
        }
        match col.pcts {
            None => c.rect(x0, 0, x1, h, crate::colors::OUTAGE, 0xd0),
            Some([min, p25, med, p75, max]) => {
                c.rect(x0, y_of(max), x1, y_of(min) + 1, white, 0x50);
                c.rect(x0, y_of(p75), x1, y_of(p25) + 1, white, 0x70);
                let y = y_of(med).min(h - 2);
                c.rect(x0, y, x1, y + 2, crate::colors::loss_rgb(col.loss), 0xff);
            }
        }
    }
    (c.w, c.h, c.px)
}

/// Battery icon for the charging mode: the charge level in a battery
/// outline, a mark at the threshold when preserving, and the mode as text
/// ("MAX", or the threshold like "80%"). Monochrome: colours mean
/// connection quality in the other icon.
pub fn render_battery(percentage: f64, preserve: bool, threshold: u32, charging: bool, height: usize) -> (usize, usize, Vec<u8>) {
    let h = height.max(GLYPH_H + 2);
    let scale = ((h - 2) / GLYPH_H).max(1);
    let body_h = (h * 3 / 5).max(6);
    let body_w = body_h * 2;
    let top = (h - body_h) / 2;
    let label = if preserve { format!("{threshold}%") } else { "MAX".to_string() };
    let text_w = label.chars().count() * (GLYPH_W + 1) * scale - scale;
    let gap = scale * 2;
    let nub = (body_h / 5).max(2);
    let w = body_w + nub + gap + text_w + scale; // a little room after the text
    let mut c = Canvas::new(w, h);
    let white = [0xf6, 0xf5, 0xf4];
    let grey = [0x9a, 0x99, 0x96];
    // outline
    c.rect(0, top, body_w, top + 1, white, 0xff);
    c.rect(0, top + body_h - 1, body_w, top + body_h, white, 0xff);
    c.rect(0, top, 1, top + body_h, white, 0xff);
    c.rect(body_w - 1, top, body_w, top + body_h, white, 0xff);
    c.rect(body_w, top + body_h / 3, body_w + nub, top + body_h - body_h / 3, white, 0xff);
    // charge level, brighter while charging
    let inner = body_w - 4;
    let fill = ((percentage.clamp(0.0, 100.0) / 100.0) * inner as f64).round() as usize;
    c.rect(2, top + 2, 2 + fill, top + body_h - 2, if charging { white } else { grey }, 0xff);
    if preserve {
        // where charging stops
        let x = 2 + (threshold.min(100) as usize * inner) / 100;
        c.rect(x, top - 1.min(top), x + 1, top + body_h + 1, white, 0xff);
    }
    let text_y = (h - GLYPH_H * scale) / 2;
    c.text(&label, w - scale, text_y, scale, white);
    (c.w, c.h, c.px)
}

pub fn rgba_to_argb(mut px: Vec<u8>) -> Vec<u8> {
    for p in px.chunks_exact_mut(4) {
        p.rotate_right(1);
    }
    px
}

/// Writes icons alternately to two files, as indicator-multiload does.
///
/// Two, not one: GNOME caches icons by path, so the path must change for a
/// new image to show. And not a new name each time: gnome-shell keeps a file
/// monitor (a main-loop source) for every icon file it ever loaded, so fresh
/// names made its main loop slower and slower (100 % CPU after a few days).
pub struct IconSink {
    dir: PathBuf,
    counter: u64,
    last: Option<(String, PathBuf)>,
}

impl IconSink {
    pub fn new(dir: &Path) -> std::io::Result<IconSink> {
        std::fs::create_dir_all(dir)?;
        for entry in std::fs::read_dir(dir)?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        Ok(IconSink { dir: dir.to_path_buf(), counter: 0, last: None })
    }

    /// The path of the icon; unchanged (and nothing written) when the image is.
    pub fn write_svg(&mut self, svg: &str) -> std::io::Result<PathBuf> {
        if let Some((last, path)) = &self.last
            && last == svg
        {
            return Ok(path.clone());
        }
        self.counter += 1;
        let path = self.dir.join(format!("icon-{}.svg", self.counter % 2));
        // write then rename, so the shell never reads a half-written file
        let tmp = self.dir.join("icon.tmp");
        std::fs::write(&tmp, svg)?;
        std::fs::rename(&tmp, &path)?;
        self.last = Some((svg.to_string(), path.clone()));
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_alternates_two_files_and_skips_identical_images() {
        let dir = std::env::temp_dir().join(format!("pingtray-sink-{}", std::process::id()));
        let mut sink = IconSink::new(&dir).unwrap();
        let a = sink.write_svg("<svg>a</svg>").unwrap();
        assert_eq!(sink.write_svg("<svg>a</svg>").unwrap(), a);
        let b = sink.write_svg("<svg>b</svg>").unwrap();
        let c = sink.write_svg("<svg>c</svg>").unwrap();
        assert_ne!(a, b);
        assert_eq!(a, c);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn column_percentiles_and_loss() {
        let c = Column::from_samples(&[Some(10.0), None, Some(30.0), Some(20.0)]);
        assert_eq!(c.pcts.unwrap()[2], 20.0);
        assert_eq!(c.loss, 0.25);
        assert!(Column::from_samples(&[None]).pcts.is_none());
    }

    #[test]
    fn renders_battery_icon() {
        let (w, h, px) = render_battery(62.0, true, 80, false, 20);
        assert_eq!(px.len(), w * h * 4);
        assert!(w > h);
    }

    #[test]
    fn renders_wide_icon() {
        let data = IconData {
            current: Current::Ms(28.0),
            columns: vec![Column::from_samples(&[Some(25.0), Some(31.0)]); 20],
        };
        let th = Thresholds { warn_ms: 60.0, bad_ms: 200.0 };
        let (w, h, px) = render(&data, 20, th);
        assert_eq!(h, 20);
        assert!(w >= 2 * h);
        assert_eq!(px.len(), w * h * 4);
    }
}
