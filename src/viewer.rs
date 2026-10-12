//! The big graph: reply times and loss over a zoomable, scrollable time range.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{Local, Offset, TimeZone};
use eframe::egui::{self, Color32, RichText};
use egui_plot::{FilledArea, GridInput, GridMark, HoverPosition, Line, Plot, PlotPoints, Points};

use crate::config::Config;
use crate::pinger::now_ms;
use crate::store::{Bucket, Store, TargetRow};

const SPANS: &[(&str, f64)] = &[
    ("5 min", 300.0),
    ("15 min", 900.0),
    ("1 h", 3600.0),
    ("6 h", 21_600.0),
    ("24 h", 86_400.0),
    ("7 d", 604_800.0),
    ("30 d", 2_592_000.0),
];
/// Candidate bucket sizes and time-axis steps, in seconds.
const STEPS: &[f64] = &[
    1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0, 7200.0,
    10_800.0, 21_600.0, 43_200.0, 86_400.0, 172_800.0, 604_800.0, 2_592_000.0,
];
const MAX_POINTS: f64 = 1200.0;
const MIN_SPAN: f64 = 30.0;
const MAX_SPAN: f64 = 400.0 * 86_400.0;
const PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x35, 0x84, 0xe4),
    Color32::from_rgb(0xe6, 0x61, 0x00),
    Color32::from_rgb(0x2e, 0xc2, 0x7e),
    Color32::from_rgb(0x91, 0x41, 0xac),
    Color32::from_rgb(0xe0, 0x1b, 0x24),
    Color32::from_rgb(0xf5, 0xc2, 0x11),
    Color32::from_rgb(0x00, 0xa5, 0xa5),
    Color32::from_rgb(0x98, 0x6a, 0x44),
];

struct TargetView {
    row: TargetRow,
    color: Color32,
    shown: bool,
}

#[derive(Clone, PartialEq)]
struct Query {
    from_ms: i64,
    to_ms: i64,
    bucket_ms: i64,
    ids: Vec<i64>,
}

struct Answer {
    query: Query,
    data: HashMap<i64, Vec<Bucket>>,
}

pub struct ViewerApp {
    interval_secs: f64,
    targets: Vec<TargetView>,
    first_sample_s: Option<f64>,
    /// Visible time range, unix seconds.
    view: (f64, f64),
    /// When set, the right edge sticks to "now".
    follow: bool,
    log_y: bool,
    band: bool,
    loaded: Option<Answer>,
    sent: Option<Query>,
    sent_at: Instant,
    req_tx: mpsc::Sender<Query>,
    ans_rx: mpsc::Receiver<Answer>,
}

fn spawn_worker(db: PathBuf, ctx: egui::Context) -> (mpsc::Sender<Query>, mpsc::Receiver<Answer>) {
    let (req_tx, req_rx) = mpsc::channel::<Query>();
    let (ans_tx, ans_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let store = match Store::open(&db) {
            Ok(s) => s,
            Err(e) => return tracing::error!("opening {}: {e}", db.display()),
        };
        while let Ok(mut q) = req_rx.recv() {
            // only the latest request matters (the user may be dragging)
            while let Ok(newer) = req_rx.try_recv() {
                q = newer;
            }
            let data = q
                .ids
                .iter()
                .map(|&id| {
                    let b = store.buckets(id, q.from_ms, q.to_ms, q.bucket_ms).unwrap_or_else(|e| {
                        tracing::error!("query: {e}");
                        Vec::new()
                    });
                    (id, b)
                })
                .collect();
            if ans_tx.send(Answer { query: q, data }).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, ans_rx)
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, db: PathBuf) -> anyhow::Result<Self> {
        let store = Store::open(&db)?;
        // Configured targets first, in config order; former targets after, hidden.
        let rows = store.targets()?;
        let mut targets: Vec<TargetView> = Vec::new();
        for t in &cfg.targets {
            if let Some(row) = rows.iter().find(|r| r.name == t.name) {
                targets.push(TargetView { row: row.clone(), color: Color32::GRAY, shown: true });
            }
        }
        for row in rows {
            if !targets.iter().any(|t| t.row.id == row.id) {
                targets.push(TargetView { row, color: Color32::GRAY, shown: false });
            }
        }
        for (i, t) in targets.iter_mut().enumerate() {
            t.color = PALETTE[i % PALETTE.len()];
        }
        let first_sample_s = store.first_sample_ms()?.map(|ms| ms as f64 / 1000.0);
        let (req_tx, ans_rx) = spawn_worker(db, cc.egui_ctx.clone());
        let now = now_ms() as f64 / 1000.0;
        Ok(ViewerApp {
            interval_secs: cfg.interval_secs,
            targets,
            first_sample_s,
            view: (now - 3600.0, now),
            follow: true,
            log_y: false,
            band: true,
            loaded: None,
            sent: None,
            sent_at: Instant::now(),
            req_tx,
            ans_rx,
        })
    }

    fn span(&self) -> f64 {
        self.view.1 - self.view.0
    }

    fn bucket_secs(&self) -> f64 {
        let want = (self.span() / MAX_POINTS).max(self.interval_secs);
        STEPS.iter().copied().find(|&s| s >= want).unwrap_or(*STEPS.last().unwrap())
    }

    /// Asks the worker for data covering the view plus a margin, when needed.
    fn request_data(&mut self) {
        let bucket = self.bucket_secs();
        let bucket_ms = (bucket * 1000.0) as i64;
        let ids: Vec<i64> = self.targets.iter().filter(|t| t.shown).map(|t| t.row.id).collect();
        let covered = self.loaded.as_ref().is_some_and(|a| {
            a.query.bucket_ms == bucket_ms
                && a.query.ids == ids
                && a.query.from_ms as f64 <= self.view.0 * 1000.0
                && a.query.to_ms as f64 >= self.view.1 * 1000.0
        });
        let refresh_live = self.follow && self.sent_at.elapsed().as_secs_f64() >= self.interval_secs.max(bucket / 4.0);
        if covered && !refresh_live {
            return;
        }
        let margin = self.span() * 0.5;
        let align = |s: f64| ((s * 1000.0) as i64).div_euclid(bucket_ms) * bucket_ms;
        let q = Query {
            from_ms: align(self.view.0 - margin),
            to_ms: if self.follow { now_ms() + bucket_ms } else { align(self.view.1 + margin) + bucket_ms },
            bucket_ms,
            ids,
        };
        if self.sent.as_ref() != Some(&q) || refresh_live {
            self.sent = Some(q.clone());
            self.sent_at = Instant::now();
            let _ = self.req_tx.send(q);
        }
    }

    fn y(&self, ms: f64) -> f64 {
        if self.log_y { ms.max(0.1).log10() } else { ms }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for &(label, secs) in SPANS {
                let active = self.follow && (self.span() - secs).abs() < 1.0;
                if ui.selectable_label(active, label).clicked() {
                    let now = now_ms() as f64 / 1000.0;
                    self.view = (now - secs, now);
                    self.follow = true;
                }
            }
            if let Some(first) = self.first_sample_s
                && ui.button("All").clicked() {
                    let now = now_ms() as f64 / 1000.0;
                    self.view = (first, now);
                    self.follow = true;
                }
            ui.separator();
            ui.checkbox(&mut self.follow, "Live");
            ui.checkbox(&mut self.log_y, "Log scale");
            ui.checkbox(&mut self.band, "Min/max");
            ui.separator();
            for t in &mut self.targets {
                let label = RichText::new(&t.row.name).color(t.color).strong();
                ui.checkbox(&mut t.shown, label).on_hover_text(&t.row.host);
            }
        });
    }

    /// Scroll wheel zooms the time axis (around the pointer), horizontal scroll pans.
    fn handle_wheel(&mut self, ui: &egui::Ui, hovered: bool, pointer_x: Option<f64>, width_px: f32) {
        if !hovered {
            return;
        }
        let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
        let factor = zoom.recip() as f64 * (-scroll.y as f64 * 0.003).exp();
        if (factor - 1.0).abs() > 1e-6 {
            let span = (self.span() * factor).clamp(MIN_SPAN, MAX_SPAN);
            if self.follow {
                self.view.0 = self.view.1 - span;
            } else {
                let anchor = pointer_x.unwrap_or((self.view.0 + self.view.1) / 2.0);
                let r = (anchor - self.view.0) / self.span();
                self.view = (anchor - r * span, anchor + (1.0 - r) * span);
            }
        }
        if scroll.x != 0.0 {
            let dx = -scroll.x as f64 / width_px.max(1.0) as f64 * self.span();
            self.view = (self.view.0 + dx, self.view.1 + dx);
            self.follow = false;
        }
    }

    fn plots(&mut self, ui: &mut egui::Ui) {
        let Some(answer) = &self.loaded else {
            ui.centered_and_justified(|ui| ui.label("Loading…"));
            return;
        };
        if self.targets.is_empty() {
            ui.centered_and_justified(|ui| ui.label("No data yet: is `pingtray` running?"));
            return;
        }
        let bucket_s = answer.query.bucket_ms as f64 / 1000.0;
        let gap = bucket_s * 2.5;
        let log_y = self.log_y;
        let view = self.view;

        // Build the drawable series once, then hand them to both plots.
        struct Series {
            name: String,
            color: Color32,
            segments: Vec<Vec<(f64, f64, f64, f64)>>, // x, avg, min, max
            loss: Vec<[f64; 2]>,
        }
        let series: Vec<Series> = self
            .targets
            .iter()
            .filter(|t| t.shown)
            .filter_map(|t| answer.data.get(&t.row.id).map(|b| (t, b)))
            .map(|(t, buckets)| {
                let mut segments: Vec<Vec<_>> = Vec::new();
                let mut prev_x = f64::NEG_INFINITY;
                let mut loss = Vec::new();
                for b in buckets {
                    let x = b.ts_ms as f64 / 1000.0 + bucket_s / 2.0;
                    if b.lost > 0 {
                        loss.push([x, b.loss_pct()]);
                    }
                    let (Some(avg), Some(min), Some(max)) = (b.avg, b.min, b.max) else {
                        prev_x = f64::NEG_INFINITY;
                        continue;
                    };
                    if x - prev_x > gap || segments.is_empty() {
                        segments.push(Vec::new());
                    }
                    segments.last_mut().unwrap().push((x, self.y(avg), self.y(min), self.y(max)));
                    prev_x = x;
                }
                Series { name: t.row.name.clone(), color: t.color, segments, loss }
            })
            .collect();

        let to_ms = move |y: f64| if log_y { 10f64.powf(y) } else { y };
        let label = move |hp: &HoverPosition<'_>| -> Option<String> {
            Some(match hp {
                HoverPosition::NearDataPoint { plot_name, position, .. } => {
                    format!("{plot_name}\n{}\n{:.1} ms", fmt_time(position.x, bucket_s), to_ms(position.y))
                }
                HoverPosition::Elsewhere { position } => fmt_time(position.x, bucket_s),
            })
        };
        let loss_label = move |hp: &HoverPosition<'_>| -> Option<String> {
            Some(match hp {
                HoverPosition::NearDataPoint { plot_name, position, .. } => {
                    format!("{plot_name}\n{}\nloss {:.0}%", fmt_time(position.x, bucket_s), position.y)
                }
                HoverPosition::Elsewhere { position } => fmt_time(position.x, bucket_s),
            })
        };

        let loss_height = 110.0;
        let rtt_height = (ui.available_height() - loss_height - 8.0).max(120.0);
        let mut new_view = None;
        let mut hovered = false;
        let mut pointer_x = None;
        let mut width_px = 1.0;

        let rtt = Plot::new("rtt")
            .height(rtt_height)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_drag([true, false])
            .allow_double_click_reset(false)
            .x_grid_spacer(time_grid)
            .x_axis_formatter(|m, _| fmt_axis(m))
            .show_x(false)
            .y_axis_formatter(move |m, _| format!("{:.0}", to_ms(m.value)).trim_end().to_string())
            .y_axis_label("ms")
            .label_formatter(label)
            .link_cursor("time", [true, false]);
        let r = rtt.show(ui, |pui| {
            if !pui.response().dragged() {
                pui.set_plot_bounds_x(view.0..=view.1);
            }
            pui.set_auto_bounds([false, true]);
            for s in &series {
                for seg in &s.segments {
                    if self.band && bucket_s > self.interval_secs && seg.len() > 1 {
                        let xs: Vec<f64> = seg.iter().map(|p| p.0).collect();
                        let lo: Vec<f64> = seg.iter().map(|p| p.2).collect();
                        let hi: Vec<f64> = seg.iter().map(|p| p.3).collect();
                        pui.add(FilledArea::new(&s.name, &xs, &lo, &hi).fill_color(s.color.gamma_multiply(0.15)));
                    }
                    let pts: Vec<[f64; 2]> = seg.iter().map(|p| [p.0, p.1]).collect();
                    if pts.len() == 1 {
                        pui.points(Points::new(&s.name, pts).color(s.color).radius(2.0));
                    } else {
                        pui.line(Line::new(&s.name, PlotPoints::from(pts)).color(s.color).width(1.5));
                    }
                }
            }
            (pui.plot_bounds(), pui.pointer_coordinate())
        });
        if r.response.dragged() {
            new_view = Some((r.inner.0.min()[0], r.inner.0.max()[0]));
        }
        if r.response.hovered() {
            hovered = true;
            pointer_x = r.inner.1.map(|p| p.x);
            width_px = r.response.rect.width();
        }
        if r.response.double_clicked() {
            self.follow = true;
        }

        let loss = Plot::new("loss")
            .height(loss_height)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_drag([true, false])
            .allow_double_click_reset(false)
            .x_grid_spacer(time_grid)
            .x_axis_formatter(|m, _| fmt_axis(m))
            .y_axis_formatter(|m, _| format!("{:.0}%", m.value))
            .y_axis_label("loss")
            .label_formatter(loss_label)
            .link_cursor("time", [true, false]);
        let r = loss.show(ui, |pui| {
            if !pui.response().dragged() {
                pui.set_plot_bounds_x(view.0..=view.1);
            }
            pui.set_auto_bounds([false, false]);
            pui.set_plot_bounds_y(0.0..=105.0);
            for s in &series {
                if !s.loss.is_empty() {
                    pui.points(Points::new(&s.name, s.loss.clone()).color(s.color).stems(0.0).radius(1.5));
                }
            }
            (pui.plot_bounds(), pui.pointer_coordinate())
        });
        if r.response.dragged() {
            new_view = Some((r.inner.0.min()[0], r.inner.0.max()[0]));
        }
        if r.response.hovered() {
            hovered = true;
            pointer_x = r.inner.1.map(|p| p.x);
            width_px = r.response.rect.width();
        }
        if r.response.double_clicked() {
            self.follow = true;
        }

        if let Some(v) = new_view {
            self.view = v;
            self.follow = false;
        }
        self.handle_wheel(ui, hovered, pointer_x, width_px);
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(a) = self.ans_rx.try_recv() {
            self.loaded = Some(a);
        }
        if self.follow {
            let now = now_ms() as f64 / 1000.0;
            let span = self.span();
            self.view = (now - span, now);
        }
        self.request_data();

        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_bar(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            let bucket = self.bucket_secs();
            ui.horizontal(|ui| {
                ui.label(format!(
                    "{} → {}   ·   one point = {}",
                    fmt_time(self.view.0, 1.0),
                    fmt_time(self.view.1, 1.0),
                    fmt_duration(bucket)
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("wheel: zoom · drag: scroll · double-click: back to live");
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| self.plots(ui));

        if self.follow {
            ui.ctx().request_repaint_after(Duration::from_secs_f64(self.interval_secs.max(1.0)));
        }
    }
}

fn local_offset(t: f64) -> f64 {
    Local
        .timestamp_opt(t as i64, 0)
        .single()
        .map(|d| d.offset().fix().local_minus_utc() as f64)
        .unwrap_or(0.0)
}

/// Time-axis marks on round local times (minutes, hours, midnights).
fn time_grid(input: GridInput) -> Vec<GridMark> {
    let (min, max) = input.bounds;
    let span = (max - min).max(1.0);
    let major = STEPS.iter().copied().find(|s| span / s <= 8.0).unwrap_or(*STEPS.last().unwrap());
    let minor = STEPS.iter().copied().find(|s| span / s <= 48.0).unwrap_or(major);
    let off = local_offset(min);
    let mut marks = Vec::new();
    for step in [minor, major] {
        let mut t = ((min + off) / step).floor() * step - off;
        while t <= max {
            if t >= min && (step == major || ((t + off) / major).fract().abs() > 1e-9) {
                marks.push(GridMark { value: t, step_size: step });
            }
            t += step;
        }
    }
    marks
}

fn fmt_axis(mark: GridMark) -> String {
    let Some(dt) = Local.timestamp_opt(mark.value.round() as i64, 0).single() else {
        return String::new();
    };
    let local = mark.value + local_offset(mark.value);
    if mark.step_size >= 86_400.0 || local.rem_euclid(86_400.0) < 0.5 {
        dt.format("%a %d/%m").to_string()
    } else if mark.step_size >= 60.0 {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%H:%M:%S").to_string()
    }
}

fn fmt_time(t: f64, resolution_s: f64) -> String {
    let Some(dt) = Local.timestamp_opt(t.round() as i64, 0).single() else {
        return String::new();
    };
    if resolution_s < 60.0 {
        dt.format("%a %d/%m %H:%M:%S").to_string()
    } else {
        dt.format("%a %d/%m %H:%M").to_string()
    }
}

fn fmt_duration(s: f64) -> String {
    match s {
        s if s < 60.0 => format!("{s:.0} s"),
        s if s < 3600.0 => format!("{:.0} min", s / 60.0),
        s if s < 86_400.0 => format!("{:.0} h", s / 3600.0),
        s => format!("{:.0} d", s / 86_400.0),
    }
}

pub fn run(cfg: Config) -> anyhow::Result<()> {
    let db = crate::config::db_path();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pingtray")
            .with_app_id("pingtray")
            .with_inner_size([1100.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "pingtray",
        options,
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, cfg, db)?))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
