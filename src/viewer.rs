//! The big graph: reply times and loss over a zoomable, scrollable time range.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::{Local, Offset, TimeZone};
use eframe::egui::{self, Color32, RichText};
use egui_plot::{GridInput, GridMark, Line, Plot, PlotPoint, PlotPoints, Span};

use crate::colors;
use crate::config::Config;
use crate::pinger::now_ms;
use crate::icon::percentile;
use crate::icon::Thresholds;
use crate::location::Fix;
use crate::map::{ColorBy, MapInput, MapView};
use crate::store::{Bucket, MAX, MEDIAN, MIN, Note, P10, P90, Store, TargetRow};
use crate::transport::{self, Segment};

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
const MAX_POINTS: f64 = 500.0;
/// Pings per point at least, so that the smoke means something.
const MIN_PINGS_PER_POINT: f64 = 5.0;
const MIN_SPAN: f64 = 30.0;
const MAX_SPAN: f64 = 400.0 * 86_400.0;

/// Window toggles, remembered between openings.
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Prefs {
    log_y: bool,
    split: bool,
    loss_colours: bool,
    show_ping: bool,
    show_http: bool,
    reference: Option<String>,
    map: bool,
    color_by: ColorBy,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { log_y: false, split: false, loss_colours: false, show_ping: true, show_http: true, reference: None, map: true, color_by: ColorBy::Quality }
    }
}

fn prefs_path() -> PathBuf {
    crate::config::data_dir().join("viewer.toml")
}

fn load_prefs() -> Prefs {
    std::fs::read_to_string(prefs_path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

/// One destination: its ping series and, when configured, its HTTP series.
struct TargetView {
    row: TargetRow,
    http: Option<TargetRow>,
    color: Color32,
    shown: bool,
}

/// One drawn series: a destination's ping or HTTP times.
struct Series<'a> {
    t: &'a TargetView,
    http: bool,
    buckets: &'a Vec<Bucket>,
    /// HTTP only: median DNS, connect, TLS and wait per bucket.
    phases: Option<&'a Vec<(i64, [f64; 4])>>,
}

impl Series<'_> {
    /// Id of the stored series (ping and HTTP have their own).
    fn id(&self) -> i64 {
        match (&self.t.http, self.http) {
            (Some(h), true) => h.id,
            _ => self.t.row.id,
        }
    }

    fn label(&self) -> String {
        format!("{} {}", self.t.row.name, if self.http { "HTTP" } else { "ping" })
    }

    /// Stable name for preferences: "name|ping" or "name|http".
    fn key(&self) -> String {
        format!("{}|{}", self.t.row.name, if self.http { "http" } else { "ping" })
    }
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
    /// Positions around the range, sorted by time.
    fixes: Vec<Fix>,
    /// HTTP series: median phases per bucket.
    phases: HashMap<i64, Vec<(i64, [f64; 4])>>,
    notes: Vec<Note>,
}

enum Msg {
    Data(Answer),
    Modes(Vec<Segment>),
}

pub struct ViewerApp {
    interval_secs: f64,
    http_interval_secs: f64,
    config_path: PathBuf,
    thresholds: Thresholds,
    max_gap_ms: i64,
    targets: Vec<TargetView>,
    first_sample_s: Option<f64>,
    /// Visible time range, unix seconds.
    view: (f64, f64),
    /// When set, the right edge sticks to "now".
    follow: bool,
    log_y: bool,
    /// One chart per target instead of one for all.
    split: bool,
    /// SmokePing look: grey smoke, median coloured by loss (one target per chart).
    loss_colours: bool,
    show_ping: bool,
    show_http: bool,
    loaded: Option<Answer>,
    segments: Vec<Segment>,
    has_fixes: bool,
    map: Option<MapView>,
    /// Time under the mouse in the charts.
    hover_x: Option<f64>,
    /// Series nearest to the mouse: drawn thicker, the others dimmed.
    highlight: Option<i64>,
    /// The series drawn in full (smoke, loss): "name|ping" or "name|http".
    reference: Option<String>,
    sent: Option<Query>,
    sent_at: Instant,
    req_tx: mpsc::Sender<Query>,
    ans_rx: mpsc::Receiver<Msg>,
    /// Debug aid: save a screenshot of the window to this path, then quit.
    snapshot: Option<(PathBuf, u32)>,
    saved_prefs: Prefs,
}

fn spawn_worker(db: PathBuf, max_gap_ms: i64, ctx: egui::Context) -> (mpsc::Sender<Query>, mpsc::Receiver<Msg>) {
    let (req_tx, req_rx) = mpsc::channel::<Query>();
    let (ans_tx, ans_rx) = mpsc::channel();
    let (modes_tx, modes_rx) = mpsc::channel::<Vec<Fix>>();

    // Transport modes may need railway data from the network: own thread,
    // so that the charts never wait for it.
    let modes_db = db.clone();
    let modes_ans = ans_tx.clone();
    let modes_ctx = ctx.clone();
    std::thread::spawn(move || {
        let Ok(store) = Store::open(&modes_db) else { return };
        let mut last_key = None;
        while let Ok(mut fixes) = modes_rx.recv() {
            while let Ok(newer) = modes_rx.try_recv() {
                fixes = newer;
            }
            let key = (fixes.len(), fixes.first().map(|f| f.ts_ms), fixes.last().map(|f| f.ts_ms));
            if last_key == Some(key) {
                continue;
            }
            last_key = Some(key);
            let segments = transport::classify(&fixes, max_gap_ms, |track| transport::rails_for(&store, track));
            if modes_ans.send(Msg::Modes(segments)).is_err() {
                return;
            }
            modes_ctx.request_repaint();
        }
    });

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
            let fixes = store.fixes(q.from_ms - max_gap_ms, q.to_ms + max_gap_ms).unwrap_or_else(|e| {
                tracing::error!("positions: {e}");
                Vec::new()
            });
            if !fixes.is_empty() {
                let _ = modes_tx.send(fixes.clone());
            }
            let phases = q
                .ids
                .iter()
                .filter_map(|&id| {
                    let p = store.http_phases(id, q.from_ms, q.to_ms, q.bucket_ms).ok()?;
                    (!p.is_empty()).then_some((id, p))
                })
                .collect();
            // notes from a while before, so the tooltip can say what applied
            let notes = store.notes(q.from_ms - 4 * 3_600_000, q.to_ms).unwrap_or_default();
            if ans_tx.send(Msg::Data(Answer { query: q, data, fixes, phases, notes })).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
    (req_tx, ans_rx)
}

impl ViewerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, config_path: PathBuf, db: PathBuf) -> anyhow::Result<Self> {
        let store = Store::open(&db)?;
        // Configured targets first, in config order; former targets after, hidden.
        let all_rows = store.targets()?;
        let http_of = |name: &str| all_rows.iter().find(|r| r.kind == "http" && r.name == name).cloned();
        let rows: Vec<TargetRow> = all_rows.iter().filter(|r| r.kind == "icmp").cloned().collect();
        let mut targets: Vec<TargetView> = Vec::new();
        for t in &cfg.targets {
            if let Some(row) = rows.iter().find(|r| r.name == t.name) {
                targets.push(TargetView { row: row.clone(), http: http_of(&row.name), color: Color32::GRAY, shown: true });
            }
        }
        for row in rows {
            if !targets.iter().any(|t| t.row.id == row.id) {
                let http = http_of(&row.name);
                targets.push(TargetView { row, http, color: Color32::GRAY, shown: false });
            }
        }
        for (i, t) in targets.iter_mut().enumerate() {
            t.color = rgb(colors::TARGETS[i % colors::TARGETS.len()]);
        }
        let first_sample_s = store.first_sample_ms()?.map(|ms| ms as f64 / 1000.0);
        let has_fixes = store.has_fixes()?;
        let prefs = load_prefs();
        let max_gap_ms = cfg.location.max_gap_ms();
        let (req_tx, ans_rx) = spawn_worker(db, max_gap_ms, cc.egui_ctx.clone());
        let now = now_ms() as f64 / 1000.0;
        Ok(ViewerApp {
            interval_secs: cfg.interval_secs,
            http_interval_secs: cfg.http_interval_secs,
            config_path,
            thresholds: Thresholds { warn_ms: cfg.warn_ms, bad_ms: cfg.bad_ms },
            max_gap_ms,
            targets,
            first_sample_s,
            // PINGTRAY_SPAN (seconds): initial range, a debug aid for snapshots
            view: (now - std::env::var("PINGTRAY_SPAN").ok().and_then(|v| v.parse().ok()).unwrap_or(900.0), now),
            follow: true,
            log_y: prefs.log_y,
            split: prefs.split,
            loss_colours: prefs.loss_colours,
            show_ping: prefs.show_ping,
            show_http: prefs.show_http,
            loaded: None,
            segments: Vec::new(),
            has_fixes,
            // no positions yet: no map, until asked for
            map: (prefs.map && has_fixes).then(|| MapView::new(&cc.egui_ctx, prefs.color_by)),
            saved_prefs: prefs.clone(),
            hover_x: None,
            highlight: None,
            reference: prefs.reference.clone(),
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
        // enough samples of the reference per point for its smoke to mean something
        let http_reference = self.show_http && self.reference.as_deref().is_some_and(|r| r.ends_with("|http"));
        let interval = if http_reference { self.http_interval_secs } else { self.interval_secs };
        let want = (self.span() / MAX_POINTS).max(interval * MIN_PINGS_PER_POINT);
        STEPS.iter().copied().find(|&s| s >= want).unwrap_or(*STEPS.last().unwrap())
    }

    /// Asks the worker for data covering the view plus a margin, when needed.
    fn request_data(&mut self) {
        let bucket = self.bucket_secs();
        let bucket_ms = (bucket * 1000.0) as i64;
        let ids: Vec<i64> = self
            .targets
            .iter()
            .filter(|t| t.shown)
            .flat_map(|t| {
                let ping = self.show_ping.then_some(t.row.id);
                let http = t.http.as_ref().filter(|_| self.show_http).map(|h| h.id);
                ping.into_iter().chain(http)
            })
            .collect();
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
            if ui.button("⚙").on_hover_text("Settings").clicked() {
                crate::tray::open_settings(&self.config_path);
            }
            if ui.button("Note…").on_hover_text("Note what the graph cannot know: crowded train, Wi-Fi on board…").clicked() {
                crate::tray::open_note(&self.config_path);
            }
            ui.checkbox(&mut self.follow, "Live");
            ui.checkbox(&mut self.log_y, "Log scale");
            ui.checkbox(&mut self.show_ping, "Ping").on_hover_text("ICMP echo: solid lines");
            if self.targets.iter().any(|t| t.http.is_some()) {
                ui.checkbox(&mut self.show_http, "HTTP")
                    .on_hover_text("Tiny HEAD request on a fresh connection: dashed lines");
            }
            ui.separator();
            ui.checkbox(&mut self.split, "Split").on_hover_text("One chart per target");
            ui.checkbox(&mut self.loss_colours, "Loss colours")
                .on_hover_text("SmokePing look: grey smoke, median coloured by packet loss (charts with one target)");
            let mut on = self.map.is_some();
            if ui.checkbox(&mut on, "Map").changed() {
                self.map = on.then(|| MapView::new(ui.ctx(), self.saved_prefs.color_by));
            }
            ui.separator();
            for t in &mut self.targets {
                let label = RichText::new(&t.row.name).color(t.color).strong();
                ui.checkbox(&mut t.shown, label).on_hover_text(&t.row.host);
            }
            ui.separator();
            // the series drawn in full (smoke, loss); also: click a curve
            let choices: Vec<(String, String)> = self
                .targets
                .iter()
                .filter(|t| t.shown)
                .flat_map(|t| {
                    let ping = self.show_ping.then(|| (format!("{}|ping", t.row.name), format!("{} ping", t.row.name)));
                    let http = (self.show_http && t.http.is_some())
                        .then(|| (format!("{}|http", t.row.name), format!("{} HTTP", t.row.name)));
                    ping.into_iter().chain(http)
                })
                .collect();
            let current = self
                .reference
                .as_ref()
                .and_then(|r| choices.iter().find(|c| &c.0 == r))
                .map(|c| c.1.clone())
                .unwrap_or_else(|| choices.first().map(|c| c.1.clone()).unwrap_or_default());
            ui.label("Reference");
            egui::ComboBox::from_id_salt("reference").selected_text(current).show_ui(ui, |ui| {
                for (key, label) in &choices {
                    if ui.selectable_label(self.reference.as_ref() == Some(key), label).clicked() {
                        self.reference = Some(key.clone());
                    }
                }
            })
            .response
            .on_hover_text("The series drawn with smoke and loss; the others are faint. Clicking a curve picks it too.");
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
        let grey = ui.visuals().strong_text_color();
        let note_color = ui.visuals().strong_text_color();
        // one chart with every target, or one chart each
        let groups: Vec<Vec<&TargetView>> =
            if self.split { shown.iter().map(|t| vec![*t]).collect() } else { vec![shown.clone()] };
        let n = groups.len();
        let header_h = 24.0;
        let strip_h = if self.segments.is_empty() { 0.0 } else { 30.0 };
        let per_plot = ((ui.available_height() - strip_h - n as f32 * header_h) / n as f32 - 4.0).max(80.0);

        let mut dragged_to = None;
        let mut hover: Option<(Option<f64>, f32)> = None;
        let mut double_clicked = false;
        let highlight = self.highlight;
        let mut new_highlight = None;
        let mut new_reference = None;

        if !self.segments.is_empty() {
            crate::map::mode_strip(ui, &self.segments, view, 48.0);
        }

        for (i, group) in groups.iter().enumerate() {
            let series: Vec<Series> = group
                .iter()
                .flat_map(|t| {
                    let ping = answer.data.get(&t.row.id).map(|b| Series { t, http: false, buckets: b, phases: None });
                    let http = t.http.as_ref().and_then(|h| {
                        answer.data.get(&h.id).map(|b| Series { t, http: true, buckets: b, phases: answer.phases.get(&h.id) })
                    });
                    ping.into_iter().chain(http)
                })
                .collect();
            let in_view = |b: &&Bucket| {
                let x = b.ts_ms as f64 / 1000.0;
                x + bucket_s >= view.0 && x <= view.1
            };
            ui.horizontal_wrapped(|ui| {
                for t in group {
                    let mine: Vec<&Series> = series.iter().filter(|s| std::ptr::eq(s.t, *t)).collect();
                    header(ui, t, &mine, &in_view);
                    ui.add_space(14.0);
                }
            });
            // the reference: the chosen series if in this chart, else its first ping
            let reference = series
                .iter()
                .position(|s| self.reference.as_deref() == Some(s.key().as_str()))
                .or_else(|| series.iter().position(|s| !s.http))
                .unwrap_or(0);
            let reference_id = series.get(reference).map(|s| s.id());
            // ping on the left axis, HTTP on its own scale on the right
            let visible_of = |http: bool| -> Vec<Vec<&Bucket>> {
                series.iter().filter(|s| s.http == http).map(|s| s.buckets.iter().filter(in_view).collect()).collect()
            };
            let (pings, https) = (visible_of(false), visible_of(true));
            let scale = Scale::new(
                (!pings.is_empty()).then(|| y_range(&pings, log_y)),
                (!https.is_empty()).then(|| y_range(&https, log_y)),
                log_y,
            );
            let (y_min, y_max) = scale.bounds();
            // the hovered curve (last frame) and its axis
            let focus = highlight.and_then(|id| series.iter().find(|s| s.id() == id)).map(|s| (s.http, s.t.color));
            let last = i + 1 == n;
            // with several targets on one chart, loss colours would be ambiguous
            let loss_colours = self.loss_colours && series.len() == 1;
            let plot = Plot::new(("rtt", group.iter().map(|t| t.row.id).collect::<Vec<_>>()))
                .height(per_plot)
                .allow_zoom(false)
                .allow_scroll(false)
                .allow_drag([true, false])
                .allow_double_click_reset(false)
                .show_axes([last, true])
                .x_grid_spacer(time_grid)
                .x_axis_formatter(|m, _| fmt_axis(m))
                .custom_y_axes(scale.axes(focus, ui.visuals().weak_text_color()))
                .y_grid_spacer(value_grid)
                // our own tooltip below, richer than the plot's text label
                .show_x(false)
                .show_y(false)
                .link_cursor("time", [true, false]);
            let r = plot.show(ui, |pui| {
                if !pui.response().dragged() {
                    pui.set_plot_bounds_x(view.0..=view.1);
                }
                pui.set_auto_bounds([false, false]);
                pui.set_plot_bounds_y(y_min..=y_max);
                // the others first, faint; the reference last, in full
                let order = (0..series.len()).filter(|&k| k != reference).chain(std::iter::once(reference));
                for k in order {
                    let Some(s) = series.get(k) else { continue };
                    let is_reference = k == reference;
                    // each target in its own colour, or SmokePing's own look:
                    // grey smoke, median coloured by loss
                    let (smoke, median) =
                        if loss_colours && is_reference { (grey, None) } else { (s.t.color, Some(s.t.color)) };
                    let emphasis = match highlight {
                        Some(id) if id == s.id() => Emphasis::Highlight,
                        _ if is_reference => Emphasis::Normal,
                        _ => Emphasis::Dim,
                    };
                    let http = s.http;
                    draw_target(pui, s.buckets, bucket_s, smoke, median, |ms| scale.y(ms, http), &s.label(), http, emphasis, is_reference);
                }
                if !loss_colours && let Some(s) = series.get(reference) {
                    draw_loss_marks(pui, std::slice::from_ref(s), bucket_s, y_min, y_max);
                }
                for note in answer.notes.iter().filter(|n| (n.ts_ms as f64 / 1000.0) >= view.0) {
                    let x = note.ts_ms as f64 / 1000.0;
                    pui.vline(
                        egui_plot::VLine::new(&note.text, x)
                            .color(note_color.gamma_multiply(0.6))
                            .style(egui_plot::LineStyle::dotted_dense())
                            .width(1.2),
                    );
                    if i == 0 {
                        pui.text(
                            egui_plot::Text::new(&note.text, PlotPoint::new(x, y_max), RichText::new(format!(" {}", note.text)).strong())
                                .color(note_color)
                                .anchor(egui::Align2::LEFT_TOP),
                        );
                    }
                }
                (pui.plot_bounds(), pui.pointer_coordinate())
            });
            if let Some((http, c)) = focus {
                // a bar along the edge of the hovered curve's axis
                let frame = *r.transform.frame();
                let x = if scale.on_right(http) { frame.right() } else { frame.left() };
                ui.painter().line_segment(
                    [egui::pos2(x, frame.top()), egui::pos2(x, frame.bottom())],
                    egui::Stroke::new(3.0, c),
                );
            }
            if r.response.dragged() {
                dragged_to = Some((r.inner.0.min()[0], r.inner.0.max()[0]));
            }
            // debug aid: PINGTRAY_SNAPSHOT_HOVER=0.8 fakes the mouse at 80 % of the
            // first chart, on its highest median, so that snapshots show the tooltip
            let fake = (i == 0 && !r.response.hovered())
                .then(|| std::env::var("PINGTRAY_SNAPSHOT_HOVER").ok()?.parse::<f64>().ok())
                .flatten()
                .and_then(|f| {
                    let x = view.0 + f * (view.1 - view.0);
                    let y = series
                        .iter()
                        .filter_map(|s| bucket_at(s.buckets, (x * 1000.0) as i64, answer.query.bucket_ms)?.pcts)
                        .map(|p| scale.y(p[MEDIAN], false))
                        .fold(f64::NEG_INFINITY, f64::max);
                    y.is_finite().then(|| PlotPoint::new(x, y))
                });
            if r.response.hovered() || fake.is_some() {
                if fake.is_none() {
                    hover = Some((r.inner.1.map(|p| p.x), r.response.rect.width()));
                }
                if let Some(p) = fake.or(r.inner.1) {
                    // the curve nearest to the mouse, on screen
                    let bucket_ms = answer.query.bucket_ms;
                    let pointer_y = r.transform.position_from_point_y(p.y);
                    new_highlight = series
                        .iter()
                        .filter_map(|s| {
                            let b = bucket_at(s.buckets, (p.x * 1000.0) as i64, bucket_ms)?;
                            let y = r.transform.position_from_point_y(scale.y(b.pcts?[MEDIAN], s.http));
                            Some(((y - pointer_y).abs(), s.id()))
                        })
                        .filter(|(d, _)| *d < 30.0)
                        .min_by(|a, b| a.0.total_cmp(&b.0))
                        .map(|(_, id)| id);
                    let place = describe_place(&answer.fixes, &self.segments, p.x, self.max_gap_ms);
                    let show = |ui: &mut egui::Ui| {
                        tooltip_ui(ui, p.x, bucket_ms, &series, new_highlight, reference_id, place.as_deref());
                        note_line(ui, &answer.notes, p.x);
                    };
                    if fake.is_some() {
                        let at = r.transform.position_from_point(&p);
                        egui::Tooltip::always_open(ui.ctx().clone(), ui.layer_id(), r.response.id, at)
                            .width(560.0)
                            .show(show);
                    } else {
                        r.response.clone().on_hover_ui_at_pointer(show);
                    }
                }
            }
            double_clicked |= r.response.double_clicked();
            // clicking a curve makes it the reference
            if r.response.clicked()
                && let Some(id) = new_highlight
                && let Some(s) = series.iter().find(|s| s.id() == id)
            {
                new_reference = Some(s.key());
            }
        }
        if let Some(key) = new_reference {
            self.reference = Some(key);
        }
        if new_highlight != self.highlight {
            self.highlight = new_highlight;
            ui.ctx().request_repaint();
        }

        if let Some(v) = dragged_to {
            self.view = v;
            self.follow = false;
        }
        if double_clicked {
            self.follow = true;
        }
        self.hover_x = hover.and_then(|h| h.0);
        if let Some((pointer_x, width)) = hover {
            self.handle_wheel(ui, pointer_x, width);
        }
    }
}

/// Destination name, then median and loss of each of its series over the
/// visible range.
fn header(ui: &mut egui::Ui, t: &TargetView, series: &[&Series], in_view: &dyn Fn(&&Bucket) -> bool) {
    let hover = match &t.http {
        Some(h) => format!("ping {}\nHTTP {}", t.row.host, h.host),
        None => t.row.host.clone(),
    };
    ui.label(RichText::new(&t.row.name).strong().color(t.color)).on_hover_text(hover);
    for s in series {
        let visible: Vec<&Bucket> = s.buckets.iter().filter(|b| in_view(b)).collect();
        let mut medians: Vec<f64> = visible.iter().filter_map(|b| b.pcts.map(|p| p[MEDIAN])).collect();
        medians.sort_by(f64::total_cmp);
        let (count, lost) = visible.iter().fold((0u64, 0u64), |(c, l), b| (c + b.count as u64, l + b.lost as u64));
        let kind = if s.http { "HTTP" } else { "ping" };
        if !medians.is_empty() {
            ui.label(format!("{kind} {}", fmt_ms(percentile(&medians, 0.5))));
        } else if count > 0 {
            ui.label(format!("{kind} –"));
        }
        if count > 0 && lost > 0 {
            let pct = 100.0 * lost as f64 / count as f64;
            let what = if s.http { "failed" } else { "loss" };
            ui.label(RichText::new(format!("{what} {pct:.1}%")).color(loss_color(pct / 100.0)));
        }
    }
}

/// Vertical range: a round value above every series' typical median, so
/// that rare spikes do not flatten the graph (they are clipped, as in
/// SmokePing) while a slower series (HTTP) still fits.
fn y_range(series: &[Vec<&Bucket>], log_y: bool) -> (f64, f64) {
    let mut typical: f64 = 0.0;
    let mut lowest = f64::INFINITY;
    for visible in series {
        let mut medians: Vec<f64> = visible.iter().filter_map(|b| b.pcts.map(|p| p[MEDIAN])).collect();
        if medians.is_empty() {
            continue;
        }
        medians.sort_by(f64::total_cmp);
        typical = typical.max(percentile(&medians, 0.9) * 1.6);
        lowest = lowest.min(medians[0]);
    }
    if typical == 0.0 {
        typical = 50.0;
    }
    let top = [1.0, 2.0, 5.0]
        .iter()
        .flat_map(|m| (0..5).map(move |e| m * 10f64.powi(e)))
        .filter(|&v| v >= typical.max(5.0))
        .fold(f64::INFINITY, f64::min);
    if log_y {
        let low = if lowest.is_finite() { lowest * 0.5 } else { 1.0 }.max(0.1);
        (low.log10(), (top * 4.0).log10())
    } else {
        (0.0, top)
    }
}

fn rgb(c: [u8; 3]) -> Color32 {
    Color32::from_rgb(c[0], c[1], c[2])
}

fn loss_color(loss: f64) -> Color32 {
    rgb(colors::loss_rgb(loss))
}

#[derive(Clone, Copy, PartialEq)]
enum Emphasis {
    Normal,
    /// The curve under the mouse.
    Highlight,
    /// The others, while one is highlighted.
    Dim,
}

/// SmokePing drawing for one series. The reference gets smoke (a vertical
/// gradient per time slot), a median line and red shading where everything
/// was lost; the others only a faint median line. Each point is drawn as an
/// interval: a step across its time slot.
#[allow(clippy::too_many_arguments)]
fn draw_target(
    pui: &mut egui_plot::PlotUi<'_>,
    buckets: &[Bucket],
    bucket_s: f64,
    smoke: Color32,
    median: Option<Color32>,
    yv: impl Fn(f64) -> f64,
    name: &str,
    http: bool,
    emphasis: Emphasis,
    reference: bool,
) {
    let fade = match emphasis {
        Emphasis::Dim => 0.45,
        _ => 1.0,
    };
    let width: f32 = match emphasis {
        Emphasis::Highlight => 3.0,
        Emphasis::Normal => 2.0,
        Emphasis::Dim => 1.2,
    };
    let edge = bucket_s * 0.001;

    // runs of buckets with replies, cut where data is missing or all lost
    let mut runs: Vec<Vec<&Bucket>> = Vec::new();
    let mut lost: Vec<(f64, f64)> = Vec::new();
    let mut prev: Option<&Bucket> = None;
    for b in buckets {
        let contiguous = prev.is_some_and(|p| b.ts_ms - p.ts_ms <= (bucket_s * 1500.0) as i64);
        if b.pcts.is_none() {
            let x0 = b.ts_ms as f64 / 1000.0;
            match lost.last_mut() {
                Some(l) if contiguous && prev.is_some_and(|p| p.pcts.is_none()) => l.1 = x0 + bucket_s,
                _ => lost.push((x0, x0 + bucket_s)),
            }
        } else if contiguous && prev.is_some_and(|p| p.pcts.is_some()) {
            runs.last_mut().expect("run started").push(b);
        } else {
            runs.push(vec![b]);
        }
        prev = Some(b);
    }

    if reference {
        let columns: Vec<Vec<crate::smoke::Column>> = runs
            .iter()
            .map(|run| {
                run.iter().map(|b| (b.ts_ms as f64 / 1000.0 + bucket_s / 2.0, b.pcts.expect("replies").map(&yv))).collect()
            })
            .collect();
        let max_alpha = if http { 0.3 } else { 0.45 };
        pui.add(crate::smoke::Smoke::new(name, columns, bucket_s, smoke, max_alpha * fade));
        for (from, to) in &lost {
            pui.add(Span::new("lost", *from..=*to).fill(loss_color(1.0).gamma_multiply(0.25)).border_width(0.0));
        }
    }

    let alpha = if reference { 1.0 } else { 0.55 * fade };
    for run in &runs {
        // steps: flat across each time slot
        let mut pts: Vec<[f64; 2]> = Vec::with_capacity(run.len() * 2);
        for b in run {
            let (x0, y) = (b.ts_ms as f64 / 1000.0, yv(b.pcts.expect("replies")[MEDIAN]));
            pts.push([x0 + edge, y]);
            pts.push([x0 + bucket_s - edge, y]);
        }
        let colours: Vec<(f64, Color32)> = run
            .iter()
            .map(|b| {
                let c = median.unwrap_or_else(|| loss_color(b.lost as f64 / b.count.max(1) as f64));
                (b.ts_ms as f64 / 1000.0, c.gamma_multiply(alpha))
            })
            .collect();
        let gradient = Arc::new(move |p: PlotPoint| {
            let i = colours.partition_point(|s| s.0 <= p.x).saturating_sub(1);
            colours.get(i).map(|s| s.1).unwrap_or(Color32::GRAY)
        });
        let w = if reference { width } else { width.min(1.4) };
        let mut line = Line::new(name, PlotPoints::from(pts)).gradient_color(gradient, false).width(w);
        if http {
            line = line.style(egui_plot::LineStyle::dashed_dense());
        }
        pui.line(line);
    }
}

/// Ping on the left axis; HTTP mapped onto the same plot from its own range,
/// labelled on the right axis.
#[derive(Clone, Copy)]
struct Scale {
    log: bool,
    /// Plot range (in axis units: ms, or log10 ms).
    main: (f64, f64),
    /// HTTP range when it has its own axis.
    http: Option<(f64, f64)>,
    /// Which kind the main (left) axis shows.
    main_is_http: bool,
}

impl Scale {
    fn new(ping: Option<(f64, f64)>, http: Option<(f64, f64)>, log: bool) -> Scale {
        match (ping, http) {
            (Some(p), Some(h)) => Scale { log, main: p, http: Some(h), main_is_http: false },
            (Some(p), None) => Scale { log, main: p, http: None, main_is_http: false },
            (None, Some(h)) => Scale { log, main: h, http: None, main_is_http: true },
            (None, None) => Scale { log, main: (0.0, 50.0), http: None, main_is_http: false },
        }
    }

    fn bounds(&self) -> (f64, f64) {
        self.main
    }

    fn units(&self, ms: f64) -> f64 {
        if self.log { ms.max(0.1).log10() } else { ms }
    }

    fn ms(&self, units: f64) -> f64 {
        if self.log { 10f64.powf(units) } else { units }
    }

    /// Plot y of a reply time.
    fn y(&self, ms: f64, http: bool) -> f64 {
        let v = self.units(ms);
        match self.http {
            Some((lo, hi)) if http => self.main.0 + (v - lo) * (self.main.1 - self.main.0) / (hi - lo),
            _ => v,
        }
    }

    /// Reply time (HTTP scale) at a plot y.
    fn http_ms(&self, y: f64) -> f64 {
        match self.http {
            Some((lo, hi)) => self.ms(lo + (y - self.main.0) * (hi - lo) / (self.main.1 - self.main.0)),
            None => self.ms(y),
        }
    }

    /// Whether a series of this kind is read on the right axis.
    fn on_right(&self, http: bool) -> bool {
        http && self.http.is_some()
    }

    /// Axes; `focus` (kind, colour) is the hovered curve: its axis takes the
    /// curve's colour, the other one steps back.
    fn axes<'a>(&self, focus: Option<(bool, Color32)>, weak: Color32) -> Vec<egui_plot::AxisHints<'a>> {
        let me = *self;
        let style = |hint: egui_plot::AxisHints<'a>, text: &str, right: bool| {
            let title = RichText::new(text).strong();
            match focus {
                Some((http, c)) if me.on_right(http) == right => {
                    hint.label(title.color(c).size(15.0)).tick_label_color(c)
                }
                Some(_) => hint.label(title.color(weak)).tick_label_color(weak),
                None => hint.label(title),
            }
        };
        let main = if self.main_is_http { "HTTP request (ms)" } else { "ICMP ping (ms)" };
        let mut axes = vec![style(
            egui_plot::AxisHints::new_y().formatter(move |m, _| fmt_ms(me.ms(m.value))).min_thickness(48.0),
            main,
            false,
        )];
        if self.http.is_some() {
            axes.push(style(
                egui_plot::AxisHints::new_y()
                    .placement(egui_plot::HPlacement::Right)
                    .formatter(move |m, _| fmt_ms(me.http_ms(m.value)))
                    .min_thickness(48.0),
                "HTTP request (ms)",
                true,
            ));
        }
        axes
    }
}

fn bucket_at(buckets: &[Bucket], ms: i64, bucket_ms: i64) -> Option<&Bucket> {
    let i = buckets.partition_point(|b| b.ts_ms <= ms).checked_sub(1)?;
    buckets.get(i).filter(|b| ms < b.ts_ms + bucket_ms)
}

/// Neutral greys for the HTTP phases: their own legend, no other meaning.
const PHASES: [(&str, [u8; 3]); 4] =
    [("DNS", [0x5e, 0x5c, 0x64]), ("connect", [0x86, 0x84, 0x8d]), ("TLS", [0xae, 0xac, 0xb4]), ("answer", [0xde, 0xdd, 0xda])];

/// A short line sample, solid for ping, dashed for HTTP.
fn swatch(ui: &mut egui::Ui, color: Color32, dashed: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 12.0), egui::Sense::hover());
    let (a, b) = (rect.left_center(), rect.right_center());
    let stroke = egui::Stroke::new(2.5, color);
    if dashed {
        ui.painter().extend(egui::Shape::dashed_line(&[a, b], stroke, 4.0, 3.0));
    } else {
        ui.painter().line_segment([a, b], stroke);
    }
}

/// The latest note before `x` (within a few hours), with how long before.
fn note_line(ui: &mut egui::Ui, notes: &[Note], x: f64) {
    let ms = (x * 1000.0) as i64;
    let Some(n) = notes.iter().rev().find(|n| n.ts_ms <= ms && ms - n.ts_ms < 4 * 3_600_000) else { return };
    let ago = (ms - n.ts_ms) / 60_000;
    let when = if ago == 0 { "just before".to_string() } else { format!("{} earlier", fmt_duration(ago as f64 * 60.0)) };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.weak("Note:");
        ui.label(RichText::new(&n.text).strong());
        ui.weak(format!("({when})"));
    });
}

/// Where an HTTP request's time went, as a stacked bar with values.
fn phase_bar(ui: &mut egui::Ui, phases: [f64; 4]) {
    let total: f64 = phases.iter().sum();
    let width = 260.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 10.0), egui::Sense::hover());
    let mut x = rect.left();
    for (v, (_, c)) in phases.iter().zip(PHASES) {
        let w = if total > 0.0 { (v / total) as f32 * width } else { 0.0 };
        let r = egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(x + w, rect.bottom()));
        ui.painter().rect_filled(r, 0.0, rgb(c));
        x += w;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (v, (label, c)) in phases.iter().zip(PHASES) {
            ui.label(RichText::new("■").color(rgb(c)));
            ui.label(RichText::new(format!("{label} {}", fmt_ms(*v))).small());
            ui.add_space(4.0);
        }
    });
}

/// The hover card: every series at that moment, the one under the mouse
/// stressed, the reference marked, where HTTP time went, and where you were.
fn tooltip_ui(
    ui: &mut egui::Ui,
    x: f64,
    bucket_ms: i64,
    series: &[Series],
    highlight: Option<i64>,
    reference: Option<i64>,
    place: Option<&str>,
) {
    let bucket_s = bucket_ms as f64 / 1000.0;
    let ms = (x * 1000.0) as i64;
    ui.horizontal(|ui| {
        ui.strong(fmt_time(x, bucket_s));
        ui.weak(format!("· one point = {}", fmt_duration(bucket_s)));
    });
    ui.add_space(2.0);
    let strong = ui.visuals().strong_text_color();
    let marked = ui.visuals().selection.bg_fill.gamma_multiply(0.6);
    let num = |ui: &mut egui::Ui, t: RichText| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| ui.label(t));
    };
    let plain = |v: f64| format!("{v:.1}");
    egui::Grid::new("tooltip").num_columns(7).spacing([14.0, 3.0]).show(ui, |ui| {
        ui.weak("ms");
        for h in ["median", "P10", "P90", "min", "max", "lost"] {
            num(ui, RichText::new(h).weak());
        }
        ui.end_row();
        for s in series {
            let hl = highlight == Some(s.id());
            let txt = |t: String| {
                if hl { RichText::new(t).strong().color(strong).background_color(marked) } else { RichText::new(t) }
            };
            ui.horizontal(|ui| {
                swatch(ui, s.t.color, s.http);
                ui.label(txt(s.label()));
                if reference == Some(s.id()) {
                    ui.label(RichText::new("reference").small().weak());
                }
            });
            match bucket_at(s.buckets, ms, bucket_ms) {
                None => {
                    num(ui, RichText::new("no data").weak());
                    for _ in 0..5 {
                        ui.label("");
                    }
                }
                Some(b) => {
                    match b.pcts {
                        None => {
                            num(ui, RichText::new("all lost").color(loss_color(1.0)));
                            for _ in 0..4 {
                                ui.label("");
                            }
                        }
                        Some(p) => {
                            for k in [MEDIAN, P10, P90, MIN, MAX] {
                                let t = txt(plain(p[k]));
                                num(ui, if k == MEDIAN { t.strong() } else { t });
                            }
                        }
                    }
                    let what = format!("{}/{}", b.lost, b.count);
                    if b.lost > 0 {
                        num(ui, RichText::new(what).color(loss_color(b.lost as f64 / b.count as f64)));
                    } else {
                        num(ui, RichText::new(what).weak());
                    }
                }
            }
            ui.end_row();
        }
    });
    // HTTP breakdown: for the highlighted HTTP series, else for all of them
    let focus_http = series.iter().any(|s| s.http && highlight == Some(s.id()));
    for s in series.iter().filter(|s| s.http && (!focus_http || highlight == Some(s.id()))) {
        let Some(phases) = s.phases else { continue };
        let i = phases.partition_point(|p| p.0 <= ms).saturating_sub(1);
        let Some((_, p)) = phases.get(i).filter(|p| ms < p.0 + bucket_ms) else { continue };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            swatch(ui, s.t.color, true);
            ui.label(format!("{}: where the time goes", s.label()));
        });
        phase_bar(ui, *p);
    }
    if let Some(place) = place {
        ui.add_space(6.0);
        ui.label(RichText::new(place).small());
    }
}

/// Where you were at time `x` (unix s): coordinates, accuracy, transport mode.
fn describe_place(fixes: &[Fix], segments: &[Segment], x: f64, max_gap_ms: i64) -> Option<String> {
    let ms = (x * 1000.0) as i64;
    let p = crate::location::estimate_at(fixes, ms, max_gap_ms)?;
    let mode = segments.iter().find(|s| s.from_ms <= ms && ms <= s.to_ms).map(|s| s.mode.label());
    Some(format!(
        "at {:.5}, {:.5} (±{:.0} m){}",
        p.lat,
        p.lon,
        p.uncertainty_m,
        mode.map(|m| format!(", {m}")).unwrap_or_default()
    ))
}

/// Combined chart: packet loss as marks along the bottom, coloured as in SmokePing.
fn draw_loss_marks(
    pui: &mut egui_plot::PlotUi<'_>,
    series: &[Series],
    bucket_s: f64,
    y_min: f64,
    y_max: f64,
) {
    let mut worst: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
    for s in series {
        for b in s.buckets.iter().filter(|b| b.lost > 0) {
            let e = worst.entry(b.ts_ms).or_insert(0.0);
            *e = e.max(b.lost as f64 / b.count.max(1) as f64);
        }
    }
    let h = (y_max - y_min) * 0.04;
    for (ts, loss) in worst {
        let (a, b) = (ts as f64 / 1000.0, ts as f64 / 1000.0 + bucket_s);
        let pts = PlotPoints::from(vec![[a, y_min], [b, y_min], [b, y_min + h], [a, y_min + h]]);
        pui.polygon(
            egui_plot::Polygon::new("loss", pts).fill_color(loss_color(loss)).stroke(egui::Stroke::NONE).allow_hover(false),
        );
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
        self.save_prefs_if_changed();
        while let Ok(msg) = self.ans_rx.try_recv() {
            match msg {
                Msg::Data(a) => {
                    self.has_fixes |= !a.fixes.is_empty();
                    self.loaded = Some(a);
                }
                Msg::Modes(s) => self.segments = s,
            }
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
                let single = self.split || self.targets.iter().filter(|t| t.shown).count() == 1;
                ui.weak(if self.loss_colours && single { "median colour = loss:" } else { "loss marks:" });
                if self.loss_colours && single {
                    ui.label(RichText::new("■ none").color(rgb(colors::GOOD)));
                }
                for (label, c) in colors::LOSS_LEGEND {
                    ui.label(RichText::new(format!("■ {label}")).color(rgb(c)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.weak("wheel: zoom · drag: scroll · double-click: live");
                });
            });
        });
        if self.map.is_some() {
            let width = ui.available_width();
            egui::Panel::right("map")
                .resizable(true)
                .default_size(width * 0.42)
                .size_range(240.0..=width * 0.8)
                .show(ui, |ui| self.map_panel(ui));
        }
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

pub fn run(cfg: Config, config_path: PathBuf) -> anyhow::Result<()> {
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
        Box::new(move |cc| Ok(Box::new(ViewerApp::new(cc, cfg, config_path, db)?))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

impl ViewerApp {
    fn save_prefs_if_changed(&mut self) {
        let prefs = Prefs {
            log_y: self.log_y,
            split: self.split,
            loss_colours: self.loss_colours,
            show_ping: self.show_ping,
            show_http: self.show_http,
            reference: self.reference.clone(),
            // without positions there is no map to show: keep the saved choice
            // without positions the map stays closed at start: keep the wish
            map: if self.has_fixes || self.map.is_some() { self.map.is_some() } else { self.saved_prefs.map },
            color_by: self.map.as_ref().map_or(self.saved_prefs.color_by, |m| m.color_by()),
        };
        if prefs != self.saved_prefs {
            self.saved_prefs = prefs.clone();
            if let Ok(text) = toml::to_string(&prefs) {
                let _ = std::fs::write(prefs_path(), text);
            }
        }
    }

    fn snapshot_step(&mut self, ui: &egui::Ui) {
        let loaded = self.loaded.is_some();
        snapshot_step(ui, &mut self.snapshot, loaded);
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

impl ViewerApp {
    /// Connection quality at a time, over the shown targets: loss colour when
    /// pings were lost, else latency colour.
    fn quality_at(&self, ts_ms: i64) -> Option<Color32> {
        let answer = self.loaded.as_ref()?;
        let bucket_ms = answer.query.bucket_ms;
        let mut worst_loss: f64 = 0.0;
        let mut worst_median: Option<f64> = None;
        let mut any = false;
        for t in self.targets.iter().filter(|t| t.shown) {
            let Some(buckets) = answer.data.get(&t.row.id) else { continue };
            let i = buckets.partition_point(|b| b.ts_ms <= ts_ms).saturating_sub(1);
            let Some(b) = buckets.get(i).filter(|b| ts_ms < b.ts_ms + bucket_ms) else { continue };
            any = true;
            worst_loss = worst_loss.max(b.lost as f64 / b.count.max(1) as f64);
            if let Some(p) = b.pcts {
                worst_median = Some(worst_median.map_or(p[MEDIAN], |m: f64| m.max(p[MEDIAN])));
            }
        }
        if !any {
            return None;
        }
        Some(rgb(colors::quality_rgb(worst_loss, worst_median, self.thresholds)))
    }

    fn map_panel(&mut self, ui: &mut egui::Ui) {
        let Some(mut map) = self.map.take() else { return };
        let empty = Vec::new();
        let fixes = self.loaded.as_ref().map_or(&empty, |a| &a.fixes);
        let quality = |ts: i64| self.quality_at(ts);
        let input = MapInput {
            fixes,
            view: self.view,
            hover: self.hover_x,
            segments: &self.segments,
            quality: &quality,
            max_gap_ms: self.max_gap_ms,
            ever: self.has_fixes,
        };
        let (clicked, setup) = map.ui(ui, &input);
        if setup {
            crate::tray::open_settings_tab(&self.config_path, "positions");
        }
        self.map = Some(map);
        if let Some(t) = clicked {
            // centre the charts on the clicked moment
            let half = self.span() / 2.0;
            self.view = (t - half, t + half);
            self.follow = false;
        }
    }
}

/// Debug aid shared by the windows: with PINGTRAY_SNAPSHOT=file.png, save a
/// screenshot once `ready` and PINGTRAY_SNAPSHOT_SECS (default 3) passed, then quit.
pub fn snapshot_step(ui: &egui::Ui, snapshot: &mut Option<(PathBuf, u32)>, ready: bool) {
    let Some((path, frames)) = snapshot else { return };
    let ctx = ui.ctx().clone();
    *frames += 1;
    let wait: f64 = std::env::var("PINGTRAY_SNAPSHOT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
    if ready && ui.input(|i| i.time) >= wait && *frames % 20 == 0 {
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
