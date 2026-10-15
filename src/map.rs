//! Map panel of the graph window: the track for the visible time range,
//! coloured by connection quality or by transport mode.

use eframe::egui::{self, Color32, Pos2, RichText, Stroke};
use walkers::{HttpOptions, HttpTiles, Map, MapMemory, lat_lon, sources::OpenStreetMap};

use std::sync::mpsc;

use crate::daily::{self, Group, ModeFilter};
use crate::location::{DEFAULT_ACC_M, Fix, estimate_at};
use crate::store::MEDIAN;
use crate::transport::{self, Mode, Segment};

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ColorBy {
    Quality,
    Transport,
}

/// Fixes closer than this are joined by a solid line; farther apart, the
/// path between them is unknown and drawn dotted.
const TRUSTED_GAP_MS: i64 = 90_000;

#[derive(Clone, Copy, PartialEq)]
enum View {
    /// The track: the chart's range, or any slice of history (slider).
    Track,
    /// Every day around one time of day, merged into cells.
    Daily,
}

const WINDOWS: [(&str, f64); 5] = [("15 min", 900.0), ("1 h", 3600.0), ("3 h", 10_800.0), ("12 h", 43_200.0), ("1 day", 86_400.0)];
const DAYS: [u32; 4] = [7, 30, 90, 365];

pub struct MapView {
    tiles: HttpTiles,
    memory: MapMemory,
    color_by: ColorBy,
    view: View,
    /// Fit the map to what is shown at the next frame.
    need_fit: bool,
    /// The chart range span the map was last fitted to (follow mode).
    fitted_span: Option<i64>,
    store: Option<crate::store::Store>,
    filter: ModeFilter,
    /// The user chose the filter: no automatic preselection any more.
    filter_touched: bool,
    // track over history
    follow_chart: bool,
    track_end: f64,
    track_window: f64,
    history: Option<History>,
    // every day
    daily: Option<daily::Params>,
    daily_result: Option<daily::Result>,
    daily_pending: Option<(daily::Params, mpsc::Receiver<daily::Result>)>,
}

/// Colour of the connection at a time (ms), when known.
type QualityFn<'a> = Box<dyn Fn(i64) -> Option<Color32> + 'a>;

/// A slice of history loaded for the slider.
struct History {
    key: (i64, i64, Option<i64>),
    fixes: Vec<Fix>,
    segments: Vec<Segment>,
    /// Quality per minute: (minute start, colour).
    quality: Vec<(i64, Color32)>,
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
    /// Whether any position was ever received.
    pub ever: bool,
    /// The reference series, for history and daily colours.
    pub reference_id: Option<i64>,
    pub thresholds: crate::icon::Thresholds,
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

fn bbox_of(points: impl Iterator<Item = (f64, f64)>) -> Option<[f64; 4]> {
    points.fold(None, |b: Option<[f64; 4]>, (lat, lon)| {
        Some(match b {
            None => [lat, lon, lat, lon],
            Some(b) => [b[0].min(lat), b[1].min(lon), b[2].max(lat), b[3].max(lon)],
        })
    })
}

fn local(ts_s: f64, fmt: &str) -> String {
    chrono::TimeZone::timestamp_opt(&chrono::Local, ts_s as i64, 0).single().map(|d| d.format(fmt).to_string()).unwrap_or_default()
}

impl MapView {
    pub fn color_by(&self) -> ColorBy {
        self.color_by
    }

    pub fn new(ctx: &egui::Context, color_by: ColorBy) -> MapView {
        let options = HttpOptions {
            cache: Some(crate::config::data_dir().join("tiles")),
            ..Default::default()
        };
        MapView {
            tiles: HttpTiles::with_options(OpenStreetMap, options, ctx.clone()),
            memory: MapMemory::default(),
            color_by,
            // PINGTRAY_SNAPSHOT_MAP=daily: start on the daily view (snapshots)
            view: if std::env::var("PINGTRAY_SNAPSHOT_MAP").as_deref() == Ok("daily") { View::Daily } else { View::Track },
            need_fit: true,
            fitted_span: None,
            store: crate::store::Store::open(&crate::config::db_path()).ok(),
            filter: ModeFilter(None),
            filter_touched: false,
            follow_chart: true,
            track_end: crate::pinger::now_ms() as f64 / 1000.0,
            track_window: 3600.0,
            history: None,
            daily: None,
            daily_result: None,
            daily_pending: None,
        }
    }

    /// Draws the panel; returns a time (unix s) when the user clicked on the
    /// track, and whether they asked to set the phone up.
    pub fn ui(&mut self, ui: &mut egui::Ui, input: &MapInput<'_>) -> (Option<f64>, bool) {
        if !input.ever {
            let mut setup = false;
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.heading("No positions yet");
                ui.add_space(6.0);
                ui.label("The map shows where each ping was sent from, using your phone's GPS.");
                ui.label("Install OwnTracks (or GPSLogger) on the phone and point it at pingtray.");
                ui.add_space(8.0);
                setup = ui.button("Set up the phone…").clicked();
            });
            return (None, setup);
        }
        self.preselect_current_mode(input);
        self.controls(ui, input);
        match self.view {
            View::Track => self.track(ui, input),
            View::Daily => {
                self.daily(ui, input);
                (None, false)
            }
        }
    }

    /// While travelling, filter on the current mode (train now: the train map).
    fn preselect_current_mode(&mut self, input: &MapInput<'_>) {
        if self.filter_touched {
            return;
        }
        let now = crate::pinger::now_ms();
        if let Some(s) = input.segments.iter().rev().find(|s| s.to_ms >= now - 10 * 60_000)
            && s.mode != Mode::Still
        {
            let wanted = ModeFilter(Some([Group::of(s.mode)].into_iter().collect()));
            if self.filter != wanted {
                self.filter = wanted;
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, input: &MapInput<'_>) {
        ui.horizontal_wrapped(|ui| {
            let before = self.view;
            ui.selectable_value(&mut self.view, View::Track, "Track");
            ui.selectable_value(&mut self.view, View::Daily, "Every day at…");
            if self.view != before {
                self.need_fit = true;
            }
            ui.separator();
            if self.view == View::Track {
                ui.selectable_value(&mut self.color_by, ColorBy::Quality, "Connection");
                ui.selectable_value(&mut self.color_by, ColorBy::Transport, "Transport");
            }
            if ui.button("Fit").clicked() {
                self.need_fit = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.weak("transport:");
            if ui.selectable_label(self.filter.0.is_none(), "all").clicked() {
                self.filter = ModeFilter(None);
                self.filter_touched = true;
            }
            for g in Group::ALL {
                let on = self.filter.0.as_ref().is_some_and(|s| s.contains(&g));
                if ui.selectable_label(on, g.label()).clicked() {
                    let mut set = self.filter.0.clone().unwrap_or_default();
                    if on {
                        set.remove(&g);
                    } else {
                        set.insert(g);
                    }
                    self.filter = ModeFilter((!set.is_empty()).then_some(set));
                    self.filter_touched = true;
                }
            }
        });
        let _ = input;
    }

    // ---- track: the chart's range, or a slice of history ----

    fn track(&mut self, ui: &mut egui::Ui, input: &MapInput<'_>) -> (Option<f64>, bool) {
        let now = crate::pinger::now_ms() as f64 / 1000.0;
        let first = self.store.as_ref().and_then(|s| s.first_fix_ms().ok().flatten()).map(|ms| ms as f64 / 1000.0);
        ui.horizontal_wrapped(|ui| {
            if ui.checkbox(&mut self.follow_chart, "Follow the chart").changed() {
                self.need_fit = true;
            }
            if !self.follow_chart {
                let lo = first.unwrap_or(now - 86_400.0);
                let slider = egui::Slider::new(&mut self.track_end, lo..=now)
                    .show_value(false)
                    .trailing_fill(true);
                if ui.add_sized([ui.available_width().min(260.0), 18.0], slider).changed() {
                    self.need_fit = false;
                }
                ui.label(local(self.track_end, "%a %d/%m %H:%M"));
                egui::ComboBox::from_id_salt("track_window")
                    .selected_text(WINDOWS.iter().find(|w| w.1 == self.track_window).map_or("1 h", |w| w.0))
                    .show_ui(ui, |ui| {
                        for (label, secs) in WINDOWS {
                            if ui.selectable_value(&mut self.track_window, secs, label).changed() {
                                self.need_fit = true;
                            }
                        }
                    });
                ui.weak("before that time");
            }
        });

        // what to draw
        let (fixes, segments, quality_of): (Vec<Fix>, Vec<Segment>, QualityFn<'_>);
        if self.follow_chart {
            let (from, to) = ((input.view.0 * 1000.0) as i64, (input.view.1 * 1000.0) as i64);
            fixes = input.fixes.iter().copied().filter(|f| f.ts_ms >= from && f.ts_ms <= to).collect();
            segments = input.segments.to_vec();
            quality_of = Box::new(|ts| (input.quality)(ts));
            // refit when the range changes (a range button), not as it slides
            let span = ((input.view.1 - input.view.0) / 60.0).round() as i64;
            if self.fitted_span != Some(span) {
                self.fitted_span = Some(span);
                self.need_fit = true;
            }
        } else {
            self.load_history(input);
            let h = self.history.as_ref();
            fixes = h.map(|h| h.fixes.clone()).unwrap_or_default();
            segments = h.map(|h| h.segments.clone()).unwrap_or_default();
            let quality = h.map(|h| h.quality.clone()).unwrap_or_default();
            quality_of = Box::new(move |ts| {
                let minute = ts.div_euclid(60_000) * 60_000;
                quality.binary_search_by_key(&minute, |q| q.0).ok().map(|i| quality[i].1)
            });
        }
        // the transport filter
        let filter = self.filter.clone();
        let shown: Vec<Fix> = if filter.0.is_some() {
            fixes.iter().copied().filter(|f| filter.allows(mode_at(&segments, f.ts_ms))).collect()
        } else {
            fixes.clone()
        };
        self.legend(ui, &segments, &shown);
        if shown.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(if fixes.is_empty() { "No position in this time range." } else { "No position for this transport." })
            });
            return (None, false);
        }
        if self.need_fit
            && let Some(b) = bbox_of(shown.iter().map(|f| (f.lat, f.lon)))
        {
            self.memory.center_at(lat_lon((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0));
            let _ = self.memory.set_zoom(fit_zoom(b, ui.available_size()));
            self.need_fit = false;
        }
        let marker = if self.follow_chart { input.hover } else { Some(self.track_end) };
        let color_by = self.color_by;
        let mut clicked = None;
        let last = *shown.last().expect("not empty");
        Map::new(Some(&mut self.tiles), &mut self.memory, lat_lon(last.lat, last.lon))
            .zoom_with_ctrl(false)
            .show(ui, |ui, response, projector, _| {
                let painter = ui.painter_at(response.rect);
                let pos = |f: &Fix| projector.project(lat_lon(f.lat, f.lon)).to_pos2();
                let px_per_m = |f: &Fix| projector.scale_pixel_per_meter(lat_lon(f.lat, f.lon));
                let color_at = |ts: i64| match color_by {
                    ColorBy::Quality => quality_of(ts).unwrap_or(Color32::GRAY),
                    ColorBy::Transport => mode_at(&segments, ts).map(mode_color).unwrap_or(Color32::GRAY),
                };
                // uncertainty first, underneath: each fix's accuracy as a faint disc
                for f in &shown {
                    let r = (f.acc_m.unwrap_or(DEFAULT_ACC_M) as f32 * px_per_m(f)).max(2.0);
                    if r > 4.0 {
                        painter.circle_filled(pos(f), r, color_at(f.ts_ms).gamma_multiply(0.12));
                    }
                }
                for w in shown.windows(2) {
                    if w[1].ts_ms - w[0].ts_ms > input.max_gap_ms {
                        continue;
                    }
                    let mid = (w[0].ts_ms + w[1].ts_ms) / 2;
                    let acc = w[0].acc_m.unwrap_or(DEFAULT_ACC_M).max(w[1].acc_m.unwrap_or(DEFAULT_ACC_M));
                    let sure = (1.0 - (acc as f32 - 20.0) / 200.0).clamp(0.3, 1.0);
                    let (a, b) = (pos(&w[0]), pos(&w[1]));
                    if w[1].ts_ms - w[0].ts_ms <= TRUSTED_GAP_MS {
                        painter.line_segment([a, b], Stroke::new(6.0, Color32::from_black_alpha((140.0 * sure) as u8)));
                        painter.line_segment([a, b], Stroke::new(3.5, color_at(mid).gamma_multiply(sure)));
                    } else {
                        // minutes between fixes: the real route is unknown
                        painter.extend(egui::Shape::dotted_line(&[a, b], color_at(mid).gamma_multiply(0.5), 7.0, 1.6));
                    }
                }
                for f in &shown {
                    painter.circle_filled(pos(f), 2.5, Color32::from_black_alpha(170));
                    painter.circle_filled(pos(f), 1.6, Color32::WHITE);
                }
                let dot = |p: Pos2, fill: Color32| {
                    painter.circle(p, 5.0, fill, Stroke::new(1.5, Color32::BLACK));
                };
                dot(pos(&shown[0]), Color32::WHITE);
                dot(pos(&shown[shown.len() - 1]), Color32::BLACK);
                if let Some(e) = marker.and_then(|t| estimate_at(&fixes, (t * 1000.0) as i64, input.max_gap_ms)) {
                    let at = lat_lon(e.lat, e.lon);
                    let p = projector.project(at).to_pos2();
                    let r = (e.uncertainty_m as f32 * projector.scale_pixel_per_meter(at)).max(6.0);
                    painter.circle(p, r, Color32::from_white_alpha(40), Stroke::new(2.0, Color32::WHITE));
                    painter.circle_filled(p, 3.0, Color32::WHITE);
                }
                if response.clicked()
                    && let Some(pointer) = response.interact_pointer_pos()
                {
                    clicked = shown
                        .iter()
                        .map(|f| (f, pos(f).distance(pointer)))
                        .filter(|(_, d)| *d < 14.0)
                        .min_by(|a, b| a.1.total_cmp(&b.1))
                        .map(|(f, _)| f.ts_ms as f64 / 1000.0);
                }
            });
        (clicked, false)
    }

    fn load_history(&mut self, input: &MapInput<'_>) {
        let to = (self.track_end * 1000.0) as i64;
        let from = to - (self.track_window * 1000.0) as i64;
        // reload when the slider moved by more than a few seconds
        let key = (from / 10_000, to / 10_000, input.reference_id);
        if self.history.as_ref().is_some_and(|h| h.key == key) {
            return;
        }
        let Some(store) = &self.store else { return };
        let fixes = store.fixes(from, to).unwrap_or_default();
        let segments = transport::classify(&fixes, input.max_gap_ms, |t| transport::rails_cached(store, t));
        let quality = input
            .reference_id
            .and_then(|id| store.buckets(id, from, to, 60_000).ok())
            .unwrap_or_default()
            .iter()
            .map(|b| {
                let c = crate::colors::quality_rgb(b.lost as f64 / b.count.max(1) as f64, b.pcts.map(|p| p[MEDIAN]), input.thresholds);
                (b.ts_ms, rgb(c))
            })
            .collect();
        self.history = Some(History { key, fixes, segments, quality });
    }

    // ---- every day at one time ----

    fn daily(&mut self, ui: &mut egui::Ui, input: &MapInput<'_>) {
        let Some(target_id) = input.reference_id else {
            ui.label("Pick a reference series in the charts first.");
            return;
        };
        let p = self.daily.get_or_insert_with(|| {
            let now = chrono::Local::now();
            let minute = chrono::Timelike::hour(&now) * 60 + chrono::Timelike::minute(&now);
            daily::Params {
                target_id,
                minute_of_day: minute / 5 * 5,
                half_window: 15,
                days: 30,
                weekdays_only: false,
                modes: ModeFilter(None),
                max_gap_ms: input.max_gap_ms,
            }
        });
        p.target_id = target_id;
        p.modes = self.filter.clone();
        ui.horizontal_wrapped(|ui| {
            let mut minute = p.minute_of_day;
            ui.add_sized(
                [ui.available_width().min(240.0), 18.0],
                egui::Slider::new(&mut minute, 0..=1435).step_by(5.0).show_value(false),
            );
            p.minute_of_day = minute;
            ui.label(RichText::new(format!("{:02}:{:02}", minute / 60, minute % 60)).strong());
            ui.label("±");
            ui.add(egui::DragValue::new(&mut p.half_window).range(5..=120).suffix(" min"));
            egui::ComboBox::from_id_salt("daily_days").selected_text(format!("last {} days", p.days)).show_ui(ui, |ui| {
                for d in DAYS {
                    ui.selectable_value(&mut p.days, d, format!("last {d} days"));
                }
            });
            ui.checkbox(&mut p.weekdays_only, "weekdays");
        });
        let wanted = p.clone();

        // compute in the background; the newest request wins
        if let Some((params, rx)) = &self.daily_pending
            && let Ok(result) = rx.try_recv()
        {
            if *params == result.params {
                let first = self.daily_result.is_none();
                self.daily_result = Some(result);
                if first {
                    self.need_fit = true;
                }
            }
            self.daily_pending = None;
        }
        let up_to_date = self.daily_result.as_ref().is_some_and(|r| r.params == wanted);
        let running = self.daily_pending.as_ref().is_some_and(|(p, _)| *p == wanted);
        if !up_to_date && !running && !ui.input(|i| i.pointer.any_down()) {
            let (tx, rx) = mpsc::channel();
            let params = wanted.clone();
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                if let Ok(store) = crate::store::Store::open(&crate::config::db_path()) {
                    let _ = tx.send(daily::compute(&store, &params));
                    ctx.request_repaint();
                }
            });
            self.daily_pending = Some((wanted.clone(), rx));
        }

        ui.horizontal_wrapped(|ui| {
            if self.daily_pending.is_some() {
                ui.spinner();
            }
            match &self.daily_result {
                Some(r) => {
                    ui.weak(format!("{} days with data, {} places", r.days_with_data, r.cells.len()));
                    ui.weak("· each cell: the pings sent from there in that window, all days merged");
                }
                None => drop(ui.weak("working…")),
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.weak("connection:");
            for (label, c) in crate::colors::RAMP_LEGEND {
                ui.label(RichText::new(format!("■ {label}")).color(rgb(c)));
            }
        });
        let Some(result) = &self.daily_result else { return };
        if result.cells.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Nothing measured at that time of day."));
            return;
        }
        if self.need_fit
            && let Some(b) = bbox_of(result.cells.iter().map(|c| (c.lat + daily::CELL_LAT / 2.0, c.lon + daily::CELL_LON / 2.0)))
        {
            self.memory.center_at(lat_lon((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0));
            let _ = self.memory.set_zoom(fit_zoom([b[0] - 0.002, b[1] - 0.003, b[2] + 0.002, b[3] + 0.003], ui.available_size()));
            self.need_fit = false;
        }
        let cells = &result.cells;
        let th = input.thresholds;
        let centre = lat_lon(cells[0].lat, cells[0].lon);
        Map::new(Some(&mut self.tiles), &mut self.memory, centre)
            .zoom_with_ctrl(false)
            .show(ui, |ui, response, projector, _| {
                let painter = ui.painter_at(response.rect);
                let corner = |lat: f64, lon: f64| projector.project(lat_lon(lat, lon)).to_pos2();
                let max_count = cells.iter().map(|c| c.count).max().unwrap_or(1) as f32;
                let mut hovered: Option<&daily::Cell> = None;
                let pointer = response.hover_pos();
                for c in cells {
                    let rect = egui::Rect::from_two_pos(corner(c.lat, c.lon), corner(c.lat + daily::CELL_LAT, c.lon + daily::CELL_LON));
                    let colour = rgb(crate::colors::quality_rgb(c.loss(), c.median_ms, th));
                    // more pings, more opaque: a single pass says less than twenty
                    let weight = 0.35 + 0.5 * (c.count as f32 / max_count).sqrt();
                    painter.rect_filled(rect.shrink(0.5), 2.0, colour.gamma_multiply(weight));
                    if pointer.is_some_and(|p| rect.contains(p)) {
                        hovered = Some(c);
                        painter.rect_stroke(rect, 2.0, Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
                    }
                }
                if let Some(c) = hovered {
                    response.clone().on_hover_ui_at_pointer(|ui| {
                        ui.strong(match c.median_ms {
                            Some(m) => format!("median {m:.0} ms"),
                            None => "nothing came back".to_string(),
                        });
                        ui.label(format!("loss {:.1} %", c.loss() * 100.0));
                        ui.weak(format!("{} pings over {} day(s)", c.count, c.days));
                    });
                }
            });
    }

    fn legend(&self, ui: &mut egui::Ui, segments: &[Segment], visible: &[Fix]) {
        ui.horizontal_wrapped(|ui| match self.color_by {
            ColorBy::Quality => {
                ui.weak("connection:");
                for (label, c) in crate::colors::RAMP_LEGEND {
                    ui.label(RichText::new(format!("■ {label}")).color(rgb(c)));
                }
            }
            ColorBy::Transport => {
                let (from, to) = (visible.first().map_or(0, |f| f.ts_ms), visible.last().map_or(0, |f| f.ts_ms));
                let mut totals: Vec<(Mode, f64, i64)> = Vec::new();
                for s in segments.iter().filter(|s| s.to_ms >= from && s.from_ms <= to) {
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
            for (i, s) in segments.iter().enumerate() {
                let (a, b) = (s.from_ms as f64 / 1000.0, s.to_ms as f64 / 1000.0);
                if b < view.0 || a > view.1 {
                    continue;
                }
                // neutral greys: the charts' colours belong to the destinations
                let shade = if i % 2 == 0 { 0x5e } else { 0x4a };
                let c = Color32::from_gray(shade);
                let pts = PlotPoints::from(vec![[a, 0.1], [b, 0.1], [b, 0.9], [a, 0.9]]);
                pui.polygon(Polygon::new(s.mode.label(), pts).fill_color(c).stroke(Stroke::NONE));
                if (b.min(view.1) - a.max(view.0)) / span > 0.06 {
                    let x = (a.max(view.0) + b.min(view.1)) / 2.0;
                    let label = match s.mode {
                        Mode::Still => s.mode.label().to_string(),
                        m => format!("{} {:.0} km/h", m.label(), s.kmh()),
                    };
                    pui.text(Text::new(s.mode.label(), egui_plot::PlotPoint::new(x, 0.5), label).color(Color32::from_gray(0xee)));
                }
            }
        });
}
