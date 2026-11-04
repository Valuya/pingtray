//! The system window: the tray icons' graphs, bigger and longer, with the
//! curves an icon has no room for (CPU by kind or per core, memory by use).

use eframe::egui::{self, Color32, RichText};
use egui_plot::{GridMark, HoverPosition, Legend, Line, Plot, PlotPoints, Polygon};

use std::path::PathBuf;
use std::time::Instant;

use crate::store::Store;
use crate::system::{Detail, Metric, bytes_short};

const RANGES: [(&str, i64); 5] =
    [("10 min", 600_000), ("1 h", 3_600_000), ("6 h", 21_600_000), ("1 day", 86_400_000), ("7 days", crate::system::KEEP_MS)];
/// Points per curve at most: one per pixel or so of a wide window.
const MAX_POINTS: usize = 900;

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Percent,
    Bytes,
    BytesPerSec,
}

/// One curve: its name, colour and how to read it from a sample.
struct Curve {
    name: String,
    color: Color32,
    width: f32,
    value: Box<dyn Fn(&Detail) -> f64>,
}

fn curve(name: &str, rgb: [u8; 3], width: f32, value: impl Fn(&Detail) -> f64 + 'static) -> Curve {
    Curve { name: name.into(), color: Color32::from_rgb(rgb[0], rgb[1], rgb[2]), width, value: Box::new(value) }
}

/// Colours that stay apart on a dark background, for per-core lines.
fn core_color(i: usize, n: usize) -> Color32 {
    let h = i as f32 / n.max(1) as f32;
    egui::ecolor::Hsva::new(h, 0.55, 0.95, 1.0).into()
}

struct SystemApp {
    store: Option<Store>,
    metric: Metric,
    range_ms: i64,
    per_core: bool,
    samples: Vec<Detail>,
    loaded: Option<(Instant, i64)>,
    snapshot: Option<(PathBuf, u32)>,
}

impl SystemApp {
    fn reload(&mut self) {
        // the collector writes every 5 s: reading more often finds nothing new
        if self.loaded.is_some_and(|(at, range)| range == self.range_ms && at.elapsed().as_secs() < 5) {
            return;
        }
        let now = crate::pinger::now_ms();
        self.samples = self.store.as_ref().and_then(|s| s.system(now - self.range_ms, now + 1).ok()).unwrap_or_default();
        self.loaded = Some((Instant::now(), self.range_ms));
    }

    fn unit(&self) -> Unit {
        match self.metric {
            Metric::Cpu | Metric::Pressure => Unit::Percent,
            Metric::Memory | Metric::Swap => Unit::Bytes,
            Metric::Network | Metric::Disk => Unit::BytesPerSec,
        }
    }

    fn curves(&self) -> Vec<Curve> {
        let hue = self.metric.rgb();
        match self.metric {
            Metric::Cpu if self.per_core => {
                let n = self.samples.last().map_or(0, |d| d.cores.len());
                (0..n)
                    .map(|i| Curve {
                        name: format!("core {i}"),
                        color: core_color(i, n),
                        width: 1.2,
                        value: Box::new(move |d: &Detail| d.cores.get(i).copied().unwrap_or(0.0)),
                    })
                    .collect()
            }
            Metric::Cpu => vec![
                curve("busy", hue, 2.5, Detail::cpu_busy),
                curve("user", [0x62, 0xa0, 0xea], 1.5, |d| d.cpu_user),
                curve("system", [0xe0, 0x1b, 0x24], 1.5, |d| d.cpu_system),
                curve("waiting for disk (iowait)", [0xf6, 0xd3, 0x2d], 1.5, |d| d.cpu_iowait),
                curve("stolen by the hypervisor", [0x9a, 0x99, 0x96], 1.0, |d| d.cpu_steal),
                curve("tasks waited for a CPU (pressure)", [0xff, 0xff, 0xff], 1.0, |d| d.psi_cpu),
            ],
            Metric::Memory => vec![
                curve("really taken", hue, 2.5, Detail::mem_used),
                curve("available (headroom)", [0x57, 0xe3, 0x89], 1.5, |d| d.mem_available),
                curve("cache, given back on demand", [0x62, 0xa0, 0xea], 1.5, |d| d.mem_cache),
                curve("swap in use", [0xa5, 0x7c, 0x5a], 1.5, |d| d.swap_zram + d.swap_disk),
                curve("installed", [0x9a, 0x99, 0x96], 1.0, |d| d.mem_total),
            ],
            Metric::Swap => vec![
                curve("in use", hue, 2.5, |d| d.swap_zram + d.swap_disk),
                curve("zram (compressed in RAM)", [0x62, 0xa0, 0xea], 1.5, |d| d.swap_zram),
                curve("RAM zram really takes", [0x57, 0xe3, 0x89], 1.5, |d| d.zram_ram),
                curve("on disk", [0xe0, 0x1b, 0x24], 1.5, |d| d.swap_disk),
                curve("all swap", [0x9a, 0x99, 0x96], 1.0, |d| d.swap_total),
            ],
            Metric::Network => vec![
                curve("in", hue, 2.0, |d| d.net_in),
                curve("out", [0xff, 0xa3, 0x48], 2.0, |d| d.net_out),
            ],
            Metric::Disk => vec![
                curve("read", [0x62, 0xa0, 0xea], 2.0, |d| d.disk_read),
                curve("written", [0xff, 0xa3, 0x48], 2.0, |d| d.disk_write),
                curve("tasks waited for I/O (pressure)", [0xff, 0xff, 0xff], 1.0, |d| d.psi_io),
            ],
            Metric::Pressure => vec![
                curve("CPU", Metric::Cpu.rgb(), 2.0, |d| d.psi_cpu),
                curve("memory", Metric::Memory.rgb(), 2.0, |d| d.psi_mem),
                curve("I/O", [0x62, 0xa0, 0xea], 2.0, |d| d.psi_io),
            ],
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for m in Metric::ALL {
                let [r, g, b] = m.rgb();
                let text = RichText::new(m.title()).color(Color32::from_rgb(r, g, b));
                ui.selectable_value(&mut self.metric, m, text);
            }
            ui.separator();
            for (label, ms) in RANGES {
                if ui.selectable_label(self.range_ms == ms, label).clicked() {
                    self.range_ms = ms;
                }
            }
            if self.metric == Metric::Cpu {
                ui.separator();
                ui.selectable_value(&mut self.per_core, false, "overall");
                ui.selectable_value(&mut self.per_core, true, "per core");
            }
        });
    }

    fn summary(&self, ui: &mut egui::Ui) {
        let Some(d) = self.samples.last() else { return };
        let g = |b: f64| format!("{:.1} G", b / 1024f64.powi(3));
        let text = match self.metric {
            Metric::Cpu => {
                let hot = d.cores.iter().filter(|c| **c >= 90.0).count();
                format!(
                    "{:.0} % busy (user {:.0}, system {:.0}), iowait {:.0} %, {hot} of {} cores above 90 %, CPU pressure {:.1} %",
                    d.cpu_busy(), d.cpu_user, d.cpu_system, d.cpu_iowait, d.cores.len(), d.psi_cpu
                )
            }
            Metric::Memory => format!(
                "{} available of {}, {} of it cache; {} really taken; memory pressure {:.1} %",
                g(d.mem_available), g(d.mem_total), g(d.mem_cache), g(d.mem_used()), d.psi_mem
            ),
            Metric::Swap => format!(
                "{} used of {}: zram {} (in {} of RAM), disk {}",
                g(d.swap_zram + d.swap_disk), g(d.swap_total), g(d.swap_zram), g(d.zram_ram), g(d.swap_disk)
            ),
            Metric::Network => format!("in {}/s, out {}/s", bytes_short(d.net_in), bytes_short(d.net_out)),
            Metric::Disk => format!(
                "read {}/s, written {}/s, I/O pressure {:.1} %",
                bytes_short(d.disk_read), bytes_short(d.disk_write), d.psi_io
            ),
            Metric::Pressure => format!("CPU {:.1} %, memory {:.1} %, I/O {:.1} % of the time something waited", d.psi_cpu, d.psi_mem, d.psi_io),
        };
        ui.label(RichText::new(text).strong());
        if self.metric != Metric::Cpu || !self.per_core {
            ui.weak("The shaded area around the main curve spans its lowest to highest value within each point.");
        }
    }

    fn plot(&self, ui: &mut egui::Ui) {
        let now = crate::pinger::now_ms() as f64 / 1000.0;
        let from = now - self.range_ms as f64 / 1000.0;
        let curves = self.curves();
        let buckets = bucketed(&self.samples, &curves, from, now);
        let unit = self.unit();
        let top = match unit {
            Unit::Percent if self.metric == Metric::Cpu => 100.0,
            _ => buckets.iter().flat_map(|b| b.max.iter().copied()).fold(0.0, f64::max).max(match unit {
                Unit::Percent => 5.0,
                Unit::Bytes => 1024f64.powi(3),
                Unit::BytesPerSec => 100e3,
            }) * 1.05,
        };
        let per_core = self.metric == Metric::Cpu && self.per_core;
        Plot::new(("system", self.metric.key(), per_core))
            .legend(Legend::default().position(egui_plot::Corner::LeftTop).follow_insertion_order(true))
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_drag(false)
            .allow_double_click_reset(false)
            .x_grid_spacer(crate::viewer::time_grid)
            .x_axis_formatter(|m, _| crate::viewer::fmt_axis(m))
            .y_axis_formatter(move |m: GridMark, _| fmt_value(m.value, unit))
            .y_grid_spacer(move |input| value_grid(input, unit))
            .label_formatter(move |pos| {
                let (name, p) = match pos {
                    HoverPosition::NearDataPoint { plot_name, position, .. } => (*plot_name, *position),
                    HoverPosition::Elsewhere { position } => ("", *position),
                };
                let at = chrono::TimeZone::timestamp_opt(&chrono::Local, p.x as i64, 0)
                    .single()
                    .map(|t| t.format("%a %H:%M:%S").to_string())
                    .unwrap_or_default();
                Some(if name.is_empty() { at } else { format!("{name}: {}\n{at}", fmt_value(p.y, unit)) })
            })
            .show(ui, |pui| {
                pui.set_auto_bounds([false, false]);
                pui.set_plot_bounds_x(from..=now);
                pui.set_plot_bounds_y(0.0..=top);
                for (k, c) in curves.iter().enumerate() {
                    // the main curve's spread within each point, as smoke
                    if k == 0 && !per_core && buckets.len() > 1 {
                        let mut band: Vec<[f64; 2]> = buckets.iter().map(|b| [b.t, b.max[k]]).collect();
                        band.extend(buckets.iter().rev().map(|b| [b.t, b.min[k]]));
                        pui.polygon(
                            Polygon::new("", PlotPoints::from(band))
                                .fill_color(c.color.gamma_multiply(0.18))
                                .stroke(egui::Stroke::NONE)
                                .allow_hover(false),
                        );
                    }
                    let pts: Vec<[f64; 2]> = buckets.iter().map(|b| [b.t, b.mean[k]]).collect();
                    pui.line(Line::new(c.name.clone(), PlotPoints::from(pts)).color(c.color).width(c.width));
                }
            });
    }
}

/// Mean, min and max of each curve over one span of time.
struct Bucket {
    t: f64,
    mean: Vec<f64>,
    min: Vec<f64>,
    max: Vec<f64>,
}

/// Samples merged so that a long range stays at MAX_POINTS per curve; a
/// short one keeps every sample.
fn bucketed(samples: &[Detail], curves: &[Curve], from_s: f64, to_s: f64) -> Vec<Bucket> {
    let span_ms = ((to_s - from_s) * 1000.0 / MAX_POINTS as f64).max(1.0) as i64;
    let mut out: Vec<Bucket> = Vec::new();
    let mut key = i64::MIN;
    let mut n = 0.0;
    for d in samples {
        let k = d.ts_ms / span_ms;
        if k != key || out.is_empty() {
            if let Some(b) = out.last_mut() {
                b.mean.iter_mut().for_each(|m| *m /= n);
            }
            key = k;
            n = 0.0;
            out.push(Bucket {
                t: d.ts_ms as f64 / 1000.0,
                mean: vec![0.0; curves.len()],
                min: vec![f64::INFINITY; curves.len()],
                max: vec![f64::NEG_INFINITY; curves.len()],
            });
        }
        let b = out.last_mut().expect("pushed");
        n += 1.0;
        for (i, c) in curves.iter().enumerate() {
            let v = (c.value)(d);
            b.mean[i] += v;
            b.min[i] = b.min[i].min(v);
            b.max[i] = b.max[i].max(v);
        }
    }
    if let Some(b) = out.last_mut() {
        b.mean.iter_mut().for_each(|m| *m /= n);
    }
    out
}

/// Round steps: 1, 2 or 5 G and so on for memory, the usual decimal ones else.
fn value_grid(input: egui_plot::GridInput, unit: Unit) -> Vec<GridMark> {
    let scale = if unit == Unit::Bytes { 1024f64.powi(3) } else { 1.0 };
    let (min, max) = (input.bounds.0 / scale, input.bounds.1 / scale);
    let span = (max - min).max(1e-9);
    let step = |target: f64| {
        let raw = span / target;
        let p = 10f64.powf(raw.log10().floor());
        [1.0, 2.0, 5.0, 10.0].into_iter().map(|k| k * p).find(|s| *s >= raw).unwrap_or(10.0 * p)
    };
    let (major, minor) = (step(6.0), step(30.0));
    let mut marks = Vec::new();
    let mut v = (min / minor).ceil() * minor;
    while v <= max + 1e-9 {
        let on_major = ((v / major).round() * major - v).abs() < minor * 0.01;
        marks.push(GridMark { value: v * scale, step_size: if on_major { major } else { minor } * scale });
        v += minor;
    }
    marks
}

fn fmt_value(v: f64, unit: Unit) -> String {
    match unit {
        Unit::Percent => format!("{v:.0} %"),
        Unit::Bytes => {
            let g = v / 1024f64.powi(3);
            if g >= 10.0 || g.fract().abs() < 1e-6 { format!("{g:.0} G") } else { format!("{g:.1} G") }
        }
        Unit::BytesPerSec => format!("{}/s", bytes_short(v)),
    }
}

impl eframe::App for SystemApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.reload();
        egui::CentralPanel::default().show(ui, |ui| {
            self.controls(ui);
            ui.add_space(4.0);
            if self.samples.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No system samples yet: the pingtray tray records them every 5 s while it runs.")
                });
            } else {
                self.summary(ui);
                ui.add_space(4.0);
                self.plot(ui);
            }
            crate::viewer::snapshot_step(ui, &mut self.snapshot, !self.samples.is_empty());
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(5));
    }
}

pub fn run(metric: Option<&str>) -> anyhow::Result<()> {
    let metric = Metric::ALL.into_iter().find(|m| Some(m.key()) == metric).unwrap_or(Metric::Cpu);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("pingtray system")
            .with_app_id("pingtray-system")
            .with_inner_size([1000.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native("pingtray system", options, Box::new(move |_| {
        Ok(Box::new(SystemApp {
            store: Store::open(&crate::config::db_path()).ok(),
            metric,
            range_ms: 3_600_000,
            // PINGTRAY_SNAPSHOT_CORES=1: start per core (snapshots)
            per_core: std::env::var_os("PINGTRAY_SNAPSHOT_CORES").is_some(),
            samples: Vec::new(),
            loaded: None,
            snapshot: std::env::var_os("PINGTRAY_SNAPSHOT").map(|p| (PathBuf::from(p), 0)),
        }))
    }))
    .map_err(|e| anyhow::anyhow!("{e}"))
}
