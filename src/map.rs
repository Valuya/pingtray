//! Map panel of the graph window: the track for the visible time range,
//! coloured by connection quality or by transport mode.

use eframe::egui::{self, Color32, Pos2, RichText, Stroke};
use walkers::{HttpOptions, HttpTiles, Map, MapMemory, lat_lon, sources::OpenStreetMap};

use crate::location::{Fix, position_at};
use crate::transport::{Mode, Segment};

#[derive(Clone, Copy, PartialEq)]
pub enum ColorBy {
    Quality,
    Transport,
}

pub struct MapView {
    tiles: HttpTiles,
    memory: MapMemory,
    color_by: ColorBy,
    /// Visible track extent the zoom was last fitted to.
    fitted: Option<[i64; 4]>,
}

pub struct MapInput<'a> {
    pub fixes: &'a [Fix],
    /// Visible time range, unix seconds.
    pub view: (f64, f64),
    /// Time under the mouse in the charts, unix seconds.
    pub hover: Option<f64>,
    pub segments: &'a [Segment],
    /// Colour for the connection at a given time (ms), if any ping covers it.
    pub quality: &'a dyn Fn(i64) -> Option<Color32>,
    pub max_gap_ms: i64,
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

pub fn mode_color(m: Mode) -> Color32 {
    rgb(m.rgb())
}

fn mode_at(segments: &[Segment], ts_ms: i64) -> Option<Mode> {
    segments.iter().find(|s| s.from_ms <= ts_ms && ts_ms <= s.to_ms).map(|s| s.mode)
}

/// Slippy-map zoom showing the box `[s, w, n, e]` in `size` pixels.
fn fit_zoom(bbox: [f64; 4], size: egui::Vec2) -> f64 {
    let [s, w, n, e] = bbox;
    let merc = |lat: f64| (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln();
    let lon_span = (e - w).max(1e-6);
    let y_span = (merc(n) - merc(s)).abs().max(1e-8);
    let z_lon = (size.x as f64 * 360.0 / (256.0 * lon_span)).log2();
    let z_lat = (size.y as f64 * std::f64::consts::TAU / (256.0 * y_span)).log2();
    (z_lon.min(z_lat) - 0.4).clamp(2.0, 17.0)
}

impl MapView {
    pub fn new(ctx: &egui::Context) -> MapView {
        let options = HttpOptions {
            cache: Some(crate::config::data_dir().join("tiles")),
            ..Default::default()
        };
        MapView {
            tiles: HttpTiles::with_options(OpenStreetMap, options, ctx.clone()),
            memory: MapMemory::default(),
            color_by: ColorBy::Quality,
            fitted: None,
        }
    }

    /// Draws the panel; returns a time (unix s) when the user clicked on the track.
    pub fn ui(&mut self, ui: &mut egui::Ui, input: &MapInput<'_>) -> Option<f64> {
        let (from, to) = ((input.view.0 * 1000.0) as i64, (input.view.1 * 1000.0) as i64);
        let visible: Vec<Fix> = input.fixes.iter().copied().filter(|f| f.ts_ms >= from && f.ts_ms <= to).collect();

        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.color_by, ColorBy::Quality, "Connection");
            ui.selectable_value(&mut self.color_by, ColorBy::Transport, "Transport");
            if ui.button("Fit").clicked() {
                self.fitted = None;
                self.memory.follow_my_position();
            }
        });
        self.legend(ui, input, &visible);

        if visible.is_empty() {
            ui.centered_and_justified(|ui| ui.label("No position in this time range."));
            return None;
        }
        let bbox = visible.iter().fold([90.0, 180.0, -90.0, -180.0], |b: [f64; 4], f| {
            [b[0].min(f.lat), b[1].min(f.lon), b[2].max(f.lat), b[3].max(f.lon)]
        });
        let center = lat_lon((bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0);
        // refit while the user has not panned, when the extent changes noticeably
        let key = bbox.map(|v| (v * 2000.0).round() as i64);
        if self.memory.detached().is_none() && self.fitted != Some(key) {
            let _ = self.memory.set_zoom(fit_zoom(bbox, ui.available_size()));
            self.fitted = Some(key);
        }

        let color_by = self.color_by;
        let mut clicked = None;
        Map::new(Some(&mut self.tiles), &mut self.memory, center)
            .zoom_with_ctrl(false)
            .show(ui, |ui, response, projector, _| {
                let painter = ui.painter_at(response.rect);
                let pos = |f: &Fix| projector.project(lat_lon(f.lat, f.lon)).to_pos2();
                for w in visible.windows(2) {
                    if w[1].ts_ms - w[0].ts_ms > input.max_gap_ms {
                        continue;
                    }
                    let mid = (w[0].ts_ms + w[1].ts_ms) / 2;
                    let color = match color_by {
                        ColorBy::Quality => (input.quality)(mid).unwrap_or(Color32::GRAY),
                        ColorBy::Transport => mode_at(input.segments, mid).map(mode_color).unwrap_or(Color32::GRAY),
                    };
                    let (a, b) = (pos(&w[0]), pos(&w[1]));
                    painter.line_segment([a, b], Stroke::new(6.0, Color32::from_black_alpha(140)));
                    painter.line_segment([a, b], Stroke::new(3.5, color));
                }
                let dot = |p: Pos2, fill: Color32| {
                    painter.circle(p, 5.0, fill, Stroke::new(1.5, Color32::BLACK));
                };
                dot(pos(&visible[0]), Color32::WHITE);
                dot(pos(&visible[visible.len() - 1]), Color32::BLACK);
                if let Some((lat, lon)) =
                    input.hover.and_then(|t| position_at(input.fixes, (t * 1000.0) as i64, input.max_gap_ms))
                {
                    let p = projector.project(lat_lon(lat, lon)).to_pos2();
                    painter.circle(p, 8.0, Color32::from_white_alpha(60), Stroke::new(2.5, Color32::WHITE));
                }
                if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    clicked = visible
                        .iter()
                        .map(|f| (f, pos(f).distance(pointer)))
                        .filter(|(_, d)| *d < 14.0)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(f, _)| f.ts_ms as f64 / 1000.0);
                }
            });
        clicked
    }

    fn legend(&self, ui: &mut egui::Ui, input: &MapInput<'_>, visible: &[Fix]) {
        ui.horizontal_wrapped(|ui| match self.color_by {
            ColorBy::Quality => {
                ui.weak("colour = loss if any, else latency:");
                for (label, c) in [
                    ("fast", [0x33, 0xd1, 0x7a]),
                    ("slow", [0xf6, 0xd3, 0x2d]),
                    ("very slow", [0xff, 0x78, 0x00]),
                    ("some loss", [0x35, 0x84, 0xe4]),
                    ("heavy loss", [0xe0, 0x1b, 0x24]),
                ] {
                    ui.label(RichText::new(format!("■ {label}")).color(rgb(c)));
                }
            }
            ColorBy::Transport => {
                let (from, to) = (visible.first().map_or(0, |f| f.ts_ms), visible.last().map_or(0, |f| f.ts_ms));
                let mut totals: Vec<(Mode, f64, i64)> = Vec::new();
                for s in input.segments.iter().filter(|s| s.to_ms >= from && s.from_ms <= to) {
                    match totals.iter_mut().find(|t| t.0 == s.mode) {
                        Some(t) => {
                            t.1 += s.km;
                            t.2 += s.to_ms - s.from_ms;
                        }
                        None => totals.push((s.mode, s.km, s.to_ms - s.from_ms)),
                    }
                }
                if totals.is_empty() {
                    ui.weak("working out transport modes…");
                }
                for (mode, km, ms) in totals {
                    let text = format!("■ {} {:.1} km, {} min", mode.label(), km, ms / 60_000);
                    ui.label(RichText::new(text).color(mode_color(mode)));
                }
            }
        });
    }
}

/// A one-line strip above the charts showing the transport mode over time.
pub fn mode_strip(
    ui: &mut egui::Ui,
    segments: &[Segment],
    view: (f64, f64),
    y_axis_width: f32,
) {
    use egui_plot::{Plot, PlotPoints, Polygon, Text};
    Plot::new("modes")
        .height(26.0)
        .show_axes([false, true])
        .show_grid(false)
        .y_axis_min_width(y_axis_width)
        .y_axis_formatter(|_, _| String::new())
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_double_click_reset(false)
        .show_x(false)
        .show_y(false)
        .show(ui, |pui| {
            pui.set_plot_bounds_x(view.0..=view.1);
            pui.set_plot_bounds_y(0.0..=1.0);
            let span = view.1 - view.0;
            for s in segments {
                let (a, b) = (s.from_ms as f64 / 1000.0, s.to_ms as f64 / 1000.0);
                if b < view.0 || a > view.1 {
                    continue;
                }
                let c = mode_color(s.mode);
                let pts = PlotPoints::from(vec![[a, 0.1], [b, 0.1], [b, 0.9], [a, 0.9]]);
                pui.polygon(Polygon::new(s.mode.label(), pts).fill_color(c.gamma_multiply(0.75)).stroke(Stroke::NONE));
                if (b.min(view.1) - a.max(view.0)) / span > 0.06 {
                    let x = (a.max(view.0) + b.min(view.1)) / 2.0;
                    let label = match s.mode {
                        Mode::Still => s.mode.label().to_string(),
                        m => format!("{} {:.0} km/h", m.label(), s.kmh()),
                    };
                    pui.text(Text::new(s.mode.label(), egui_plot::PlotPoint::new(x, 0.5), label).color(Color32::BLACK));
                }
            }
        });
}
