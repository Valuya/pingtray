//! Renders the tiny bar graph shown in the tray.

/// One bar: `None` = no data yet, `Some(None)` = lost, `Some(Some(ms))` = reply time.
pub type Bar = Option<Option<f64>>;

#[derive(Clone, Copy)]
pub struct Thresholds {
    pub warn_ms: f64,
    pub bad_ms: f64,
}

const GOOD: [u8; 3] = [0x33, 0xd1, 0x7a];
const WARN: [u8; 3] = [0xf6, 0xd3, 0x2d];
const BAD: [u8; 3] = [0xff, 0x78, 0x00];
const LOST: [u8; 3] = [0xe0, 0x1b, 0x24];
const BASELINE: [u8; 4] = [0x9a, 0x99, 0x96, 0x90];

/// Bar height as a fraction of the icon, log scale from 1 ms to 1 s.
fn height_frac(ms: f64) -> f64 {
    (ms.max(1.0).log10() / 3.0).clamp(0.0, 1.0)
}

/// RGBA pixels, `size`×`size`, oldest bar on the left.
pub fn render_rgba(bars: &[Bar], size: usize, th: Thresholds) -> Vec<u8> {
    let mut px = vec![0u8; size * size * 4];
    let mut put = |x: usize, y: usize, c: [u8; 4]| {
        let i = (y * size + x) * 4;
        px[i..i + 4].copy_from_slice(&c);
    };
    let n = bars.len().max(1);
    let min_h = (size / 10).max(2);
    for (i, bar) in bars.iter().enumerate() {
        let x0 = i * size / n;
        let x1 = ((i + 1) * size / n).max(x0 + 1);
        let (h, rgb) = match bar {
            None => (0, [0, 0, 0]),
            Some(None) => (size, LOST),
            Some(Some(ms)) => {
                let h = ((height_frac(*ms) * size as f64).round() as usize).clamp(min_h, size);
                let rgb = if *ms >= th.bad_ms { BAD } else if *ms >= th.warn_ms { WARN } else { GOOD };
                (h, rgb)
            }
        };
        for x in x0..x1.min(size) {
            if h == 0 {
                put(x, size - 1, BASELINE);
            }
            for y in size - h..size {
                put(x, y, [rgb[0], rgb[1], rgb[2], 0xff]);
            }
        }
    }
    px
}

/// Same as [`render_rgba`] in ARGB32 network byte order (StatusNotifierItem pixmaps).
pub fn render_argb(bars: &[Bar], size: usize, th: Thresholds) -> Vec<u8> {
    let mut px = render_rgba(bars, size, th);
    for p in px.chunks_exact_mut(4) {
        p.rotate_right(1);
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_bar_fills_the_column() {
        let th = Thresholds { warn_ms: 50.0, bad_ms: 200.0 };
        let px = render_rgba(&[Some(None), None], 4, th);
        // left half fully red, right half only the baseline pixel
        assert_eq!(&px[0..4], &[0xe0, 0x1b, 0x24, 0xff]);
        assert_eq!(px[(0 * 4 + 3) * 4 + 3], 0);
        assert_eq!(&px[(3 * 4 + 3) * 4..(3 * 4 + 3) * 4 + 4], &BASELINE);
    }
}
