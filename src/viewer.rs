//! The big graph: reply times and loss over a zoomable, scrollable time range.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{Local, Offset, TimeZone};
use eframe::egui::{self, Color32, RichText};
use egui_plot::{FilledArea, GridInput, GridMark, HoverPosition, Line, Plot, PlotPoint, PlotPoints, Span};

use crate::config::Config;
use crate::pinger::now_ms;
use crate::icon::percentile;
use crate::store::{Bucket, MEDIAN, Store, TargetRow};

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
    loaded: Option<Answer>,
    sent: Option<Query>,
    sent_at: Instant,
    req_tx: mpsc::Sender<Query>,
    ans_rx: mpsc::Receiver<Answer>,
    /// Debug aid: save a screenshot of the window to this path, then quit.
    snapshot: Option<(PathBuf, u32)>,
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
            loaded: None,
            sent: None,
            sent_at: Instant::now(),
            req_tx,
            ans_rx,
            snapshot: std::env::var_os("PINGTRAY_SNAPSHOT").map(|p| (PathBuf::from(p), 0)),
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
            ui.separator();
            for t in &mut self.targets {
                let label = RichText::new(&t.row.name).color(t.color).strong();
                ui.checkbox(&mut t.shown, label).on_hover_text(&t.row.host);
            }
        });
    }

    /// Scroll wheel zooms the time axis (around the pointer), horizontal scroll pans.
    fn handle_wheel(&mut self, ui: &egui::Ui, pointer_x: Option<f64>, width_px: f32) {
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
        let shown: Vec<&TargetView> = self.targets.iter().filter(|t| t.shown).collect();
        if self.targets.is_empty() {
            ui.centered_and_justified(|ui| ui.label("No data yet: is `pingtray` running?"));
            return;
        }
        if shown.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Tick a target above."));
            return;
        }
        let bucket_s = answer.query.bucket_ms as f64 / 1000.0;
        let view = self.view;
        let log_y = self.log_y;
        let smoke = ui.visuals().strong_text_color();
        let n = shown.len();
        let header_h = 24.0;
        let per_plot = ((ui.available_height() - n as f32 * header_h) / n as f32 - 4.0).max(80.0);

        let mut dragged_to = None;
        let mut hover: Option<(Option<f64>, f32)> = None;
        let mut double_clicked = false;

        {
            for (i, t) in shown.iter().enumerate() {
                let empty = Vec::new();
                let buckets = answer.data.get(&t.row.id).unwrap_or(&empty);
                let visible: Vec<&Bucket> = buckets
                    .iter()
                    .filter(|b| {
                        let x = b.ts_ms as f64 / 1000.0;
                        x + bucket_s >= view.0 && x <= view.1
                    })
                    .collect();
                header(ui, t, &visible);

                let (y_min, y_max) = y_range(&visible, log_y);
                let to_ms = move |y: f64| if log_y { 10f64.powf(y) } else { y };
                let yv = move |ms: f64| if log_y { ms.max(0.1).log10() } else { ms };
                let last = i + 1 == n;
                let hover_text = |hp: &HoverPosition<'_>| -> Option<String> {
                    let x = match hp {
                        HoverPosition::NearDataPoint { position, .. } => position.x,
                        HoverPosition::Elsewhere { position } => position.x,
                    };
                    Some(describe_bucket(buckets, x, bucket_s))
                };
                let plot = Plot::new(("rtt", t.row.id))
                    .height(per_plot)
                    .allow_zoom(false)
                    .allow_scroll(false)
                    .allow_drag([true, false])
                    .allow_double_click_reset(false)
                    .show_axes([last, true])
                    .x_grid_spacer(time_grid)
                    .x_axis_formatter(|m, _| fmt_axis(m))
                    .y_axis_formatter(move |m, _| fmt_ms(to_ms(m.value)))
                    .y_grid_spacer(value_grid)
                    .y_axis_min_width(48.0)
                    .label_formatter(hover_text)
                    .link_cursor("time", [true, false]);
                let r = plot.show(ui, |pui| {
                    if !pui.response().dragged() {
                        pui.set_plot_bounds_x(view.0..=view.1);
                    }
                    pui.set_auto_bounds([false, false]);
                    pui.set_plot_bounds_y(y_min..=y_max);
                    draw_target(pui, buckets, bucket_s, smoke, yv, &t.row.name);
                    (pui.plot_bounds(), pui.pointer_coordinate())
                });
                if r.response.dragged() {
                    dragged_to = Some((r.inner.0.min()[0], r.inner.0.max()[0]));
                }
                if r.response.hovered() {
                    hover = Some((r.inner.1.map(|p| p.x), r.response.rect.width()));
                }
                double_clicked |= r.response.double_clicked();
            }
        }

        if let Some(v) = dragged_to {
            self.view = v;
            self.follow = false;
        }
        if double_clicked {
            self.follow = true;
        }
        if let Some((pointer_x, width)) = hover {
            self.handle_wheel(ui, pointer_x, width);
        }
    }
}

/// Target name with median and loss over the visible range.
fn header(ui: &mut egui::Ui, t: &TargetView, visible: &[&Bucket]) {
    let mut medians: Vec<f64> = visible.iter().filter_map(|b| b.pcts.map(|p| p[MEDIAN])).collect();
    medians.sort_by(f64::total_cmp);
    let (count, lost) = visible.iter().fold((0u64, 0u64), |(c, l), b| (c + b.count as u64, l + b.lost as u64));
    ui.horizontal(|ui| {
        ui.label(RichText::new(&t.row.name).strong().color(t.color));
        ui.weak(&t.row.host);
        if !medians.is_empty() {
            ui.label(format!("median {}", fmt_ms(percentile(&medians, 0.5))));
        }
        if count > 0 {
            let pct = 100.0 * lost as f64 / count as f64;
            ui.label(RichText::new(format!("loss {pct:.2}%")).color(loss_color(pct / 100.0)));
        }
    });
}

/// Vertical range: a round value above typical medians, so that rare spikes
/// do not flatten the graph (they are clipped, as in SmokePing).
fn y_range(visible: &[&Bucket], log_y: bool) -> (f64, f64) {
    let mut p75: Vec<f64> = visible.iter().filter_map(|b| b.pcts.map(|p| p[MEDIAN])).collect();
    p75.sort_by(f64::total_cmp);
    let typical = if p75.is_empty() { 50.0 } else { percentile(&p75, 0.9) * 1.8 };
    let top = [1.0, 2.0, 5.0]
        .iter()
        .flat_map(|m| (0..5).map(move |e| m * 10f64.powi(e)))
        .filter(|&v| v >= typical.max(5.0))
        .fold(f64::INFINITY, f64::min);
    if log_y {
        let low = p75.first().map(|v| v * 0.5).unwrap_or(1.0).max(0.1);
        (low.log10(), (top * 4.0).log10())
    } else {
        (0.0, top)
    }
}

fn loss_color(loss: f64) -> Color32 {
    let [r, g, b] = crate::icon::loss_rgb(loss);
    Color32::from_rgb(r, g, b)
}

/// SmokePing drawing for one target: grey bands for the spread, a median
/// step line coloured by loss, red shading where every ping was lost.
fn draw_target(
    pui: &mut egui_plot::PlotUi<'_>,
    buckets: &[Bucket],
    bucket_s: f64,
    smoke: Color32,
    yv: impl Fn(f64) -> f64,
    name: &str,
) {
    let edge = bucket_s * 0.001;
    let mut lost_from: Option<f64> = None;
    let mut segment: Vec<&Bucket> = Vec::new();
    let flush_lost = |pui: &mut egui_plot::PlotUi<'_>, from: f64, to: f64| {
        pui.add(
            Span::new("lost", from..=to)
                .fill(loss_color(1.0).gamma_multiply(0.35))
                .border_width(0.0),
        );
    };
    let flush_segment = |pui: &mut egui_plot::PlotUi<'_>, seg: &[&Bucket]| {
        if seg.is_empty() {
            return;
        }
        let mut xs = Vec::with_capacity(seg.len() * 2);
        for b in seg {
            let x0 = b.ts_ms as f64 / 1000.0;
            xs.push(x0 + edge);
            xs.push(x0 + bucket_s - edge);
        }
        let col = |k: usize| -> Vec<f64> {
            seg.iter().flat_map(|b| {
                let v = yv(b.pcts.unwrap()[k]);
                [v, v]
            }).collect()
        };
        for (lo, hi, alpha) in [(0, 6, 0.10), (1, 5, 0.14), (2, 4, 0.22)] {
            pui.add(
                FilledArea::new(name, &xs, &col(lo), &col(hi))
                    .fill_color(smoke.gamma_multiply(alpha))
                    .stroke(egui::Stroke::NONE)
                    .allow_hover(false),
            );
        }
        let pts: Vec<[f64; 2]> = xs.iter().copied().zip(col(MEDIAN)).map(|(x, y)| [x, y]).collect();
        let starts: Vec<(f64, Color32)> = seg
            .iter()
            .map(|b| (b.ts_ms as f64 / 1000.0, loss_color(b.lost as f64 / b.count.max(1) as f64)))
            .collect();
        let gradient = Arc::new(move |p: PlotPoint| {
            let i = starts.partition_point(|s| s.0 <= p.x).saturating_sub(1);
            starts.get(i).map(|s| s.1).unwrap_or(Color32::GRAY)
        });
        pui.line(Line::new(name, PlotPoints::from(pts)).gradient_color(gradient, false).width(2.0));
    };

    let mut prev_end = f64::NEG_INFINITY;
    for b in buckets {
        let x0 = b.ts_ms as f64 / 1000.0;
        let gap = x0 - prev_end > bucket_s * 0.5;
        if b.pcts.is_none() {
            flush_segment(pui, &segment);
            segment.clear();
            if gap {
                if let Some(from) = lost_from.take() {
                    flush_lost(pui, from, prev_end);
                }
            }
            lost_from.get_or_insert(x0);
        } else {
            if let Some(from) = lost_from.take() {
                flush_lost(pui, from, prev_end);
            }
            if gap {
                flush_segment(pui, &segment);
                segment.clear();
            }
            segment.push(b);
        }
        prev_end = x0 + bucket_s;
    }
    flush_segment(pui, &segment);
    if let Some(from) = lost_from {
        flush_lost(pui, from, prev_end);
    }
}

fn describe_bucket(buckets: &[Bucket], x: f64, bucket_s: f64) -> String {
    let t = fmt_time(x, bucket_s);
    let ms = (x * 1000.0) as i64;
    let i = buckets.partition_point(|b| b.ts_ms <= ms).saturating_sub(1);
    match buckets.get(i).filter(|b| ms < b.ts_ms + (bucket_s * 1000.0) as i64) {
        None => format!("{t}\nno data"),
        Some(b) => match b.pcts {
            None => format!("{t}\n{} pings, all lost", b.count),
            Some(p) => format!(
                "{t}\nmedian {}\n10–90%: {} – {}\nmin/max: {} – {}\nloss {:.1}% of {}",
                fmt_ms(p[MEDIAN]),
                fmt_ms(p[1]),
                fmt_ms(p[5]),
                fmt_ms(p[0]),
                fmt_ms(p[6]),
                b.loss_pct(),
                b.count
            ),
        },
    }
}

fn fmt_ms(ms: f64) -> String {
    match ms {
        0.0 => "0".to_string(),
        m if m < 10.0 => format!("{m:.1} ms"),
        m if m < 10_000.0 => format!("{m:.0} ms"),
        m => format!("{:.0} s", m / 1000.0),
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.snapshot_step(ui);
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
                    "{}  to  {}   ·   one point = {}",
                    fmt_time(self.view.0, 1.0),
                    fmt_time(self.view.1, 1.0),
                    fmt_duration(bucket)
                ));
                ui.separator();
                ui.weak("median colour = loss:");
                for (label, loss) in [("0", 0.0), ("<5%", 0.01), ("<10%", 0.07), ("<20%", 0.15), ("<50%", 0.3), ("more", 0.6)] {
                    ui.label(RichText::new(format!("■ {label}")).color(loss_color(loss)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("wheel: zoom · drag: scroll · double-click: live");
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

impl ViewerApp {
    fn snapshot_step(&mut self, ui: &egui::Ui) {
        let loaded = self.loaded.is_some();
        let Some((path, frames)) = &mut self.snapshot else { return };
        let ctx = ui.ctx().clone();
        *frames += 1;
        if *frames == 30 || (*frames > 30 && *frames % 50 == 0 && loaded) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            let [w, h] = img.size;
            let res = std::fs::File::create(&*path).map_err(anyhow::Error::from).and_then(|f| {
                let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
                enc.set_color(png::ColorType::Rgba);
                enc.write_header()?.write_image_data(&rgba)?;
                Ok(())
            });
            if let Err(e) = res {
                tracing::error!("snapshot: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

/// Value-axis marks on 1-2-5 steps, about four labelled ones per plot.
fn value_grid(input: GridInput) -> Vec<GridMark> {
    let (min, max) = input.bounds;
    let span = (max - min).max(1e-9);
    let nice = |target: f64| {
        let base = 10f64.powf(target.log10().floor());
        [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * base).find(|&s| s >= target).unwrap()
    };
    let major = nice(span / 4.0);
    let minor = major / if (major / 10f64.powf(major.log10().floor()) - 2.0).abs() < 1e-9 { 4.0 } else { 5.0 };
    let mut marks = Vec::new();
    for step in [minor, major] {
        let mut v = (min / step).ceil() * step;
        while v <= max + 1e-9 {
            let on_major = ((v / major).round() - v / major).abs() < 1e-6;
            if step == major || !on_major {
                marks.push(GridMark { value: v, step_size: step });
            }
            v += step;
        }
    }
    marks
}
