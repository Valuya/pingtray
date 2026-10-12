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

/// SmokePing-like colours for the lost fraction of a column.
pub fn loss_rgb(loss: f64) -> [u8; 3] {
    match loss {
        l if l <= 0.0 => [0x33, 0xd1, 0x7a], // green
        l if l < 0.05 => [0x33, 0xc7, 0xde], // cyan
        l if l < 0.10 => [0x35, 0x84, 0xe4], // blue
        l if l < 0.20 => [0xa3, 0x47, 0xba], // purple
        l if l < 0.50 => [0xff, 0x78, 0x00], // orange
        _ => [0xe0, 0x1b, 0x24],             // red
    }
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
        Current::Lost => ("--".to_string(), [0xe0, 0x1b, 0x24]),
        Current::Ms(ms) => {
            let label = match ms {
                m if m < 9.95 => format!("{m:.1}"),
                m if m < 999.5 => format!("{m:.0}"),
                m => format!("{:.0}k", (m / 1000.0).min(99.0)),
            };
            let rgb = if ms >= th.bad_ms {
                [0xff, 0x78, 0x00]
            } else if ms >= th.warn_ms {
                [0xf6, 0xd3, 0x2d]
            } else {
                [0xf6, 0xf5, 0xf4]
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
            None => c.rect(x0, 0, x1, h, loss_rgb(1.0), 0xb0),
            Some([min, p25, med, p75, max]) => {
                c.rect(x0, y_of(max), x1, y_of(min) + 1, white, 0x50);
                c.rect(x0, y_of(p75), x1, y_of(p25) + 1, white, 0x70);
                let y = y_of(med).min(h - 2);
                c.rect(x0, y, x1, y + 2, loss_rgb(col.loss), 0xff);
            }
        }
    }
    (c.w, c.h, c.px)
}

pub fn rgba_to_argb(mut px: Vec<u8>) -> Vec<u8> {
    for p in px.chunks_exact_mut(4) {
        p.rotate_right(1);
    }
    px
}

/// Writes the icon as PNG under a fresh name (GNOME caches icons by path) and
/// removes the previous files.
pub struct PngSink {
    dir: PathBuf,
    counter: u64,
}

impl PngSink {
    pub fn new(dir: &Path) -> std::io::Result<PngSink> {
        std::fs::create_dir_all(dir)?;
        for entry in std::fs::read_dir(dir)?.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        Ok(PngSink { dir: dir.to_path_buf(), counter: 0 })
    }

    pub fn write(&mut self, w: usize, h: usize, rgba: &[u8]) -> std::io::Result<PathBuf> {
        self.counter += 1;
        let path = self.dir.join(format!("icon-{}.png", self.counter));
        let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        let mut enc = png::Encoder::new(file, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(rgba)?;
        // keep the previous one: the shell may still be loading it
        if self.counter > 2 {
            let _ = std::fs::remove_file(self.dir.join(format!("icon-{}.png", self.counter - 2)));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_percentiles_and_loss() {
        let c = Column::from_samples(&[Some(10.0), None, Some(30.0), Some(20.0)]);
        assert_eq!(c.pcts.unwrap()[2], 20.0);
        assert_eq!(c.loss, 0.25);
        assert!(Column::from_samples(&[None]).pcts.is_none());
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
