//! Long-running process: pings, stores history, feeds the tray icon.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::{self, Config};
use crate::icon::{self, Column, Current, IconData, PngSink, Thresholds};
use crate::pinger::{self, Kind, Sample};
use crate::store::Store;
use crate::tray::{TrayHandle, TrayState};

/// Recent samples per target, for the icon and the tooltip.
struct Live {
    names: Vec<String>,
    recent: Vec<VecDeque<(i64, Option<f64>)>>,
    /// Last HTTP probe per target: total ms, or `None` when it failed.
    last_http: Vec<Option<Option<f64>>>,
    keep_ms: i64,
    columns: usize,
    column_ms: i64,
    /// Window for the number shown in the icon.
    current_ms: i64,
}

const TOOLTIP_WINDOW_MS: i64 = 300_000;

impl Live {
    fn new(cfg: &Config) -> Live {
        let column_ms = (cfg.icon_column_secs * 1000.0) as i64;
        Live {
            names: cfg.targets.iter().map(|t| t.name.clone()).collect(),
            recent: vec![VecDeque::new(); cfg.targets.len()],
            last_http: vec![None; cfg.targets.len()],
            keep_ms: TOOLTIP_WINDOW_MS.max(column_ms * cfg.icon_columns as i64),
            columns: cfg.icon_columns,
            column_ms,
            current_ms: ((cfg.interval_secs * 2.5).max(5.0) * 1000.0) as i64,
        }
    }

    fn push_http(&mut self, s: &Sample) {
        self.last_http[s.target] = Some(s.rtt.map(|d| d.as_secs_f64() * 1000.0));
    }

    fn push(&mut self, s: &Sample) {
        let q = &mut self.recent[s.target];
        q.push_back((s.ts_ms, s.rtt.map(|d| d.as_secs_f64() * 1000.0)));
        while q.front().is_some_and(|(ts, _)| *ts < s.ts_ms - self.keep_ms) {
            q.pop_front();
        }
    }

    /// Samples of one target (`Some(index)`) or of all of them, in a time range.
    fn pooled(&self, which: Option<usize>, from_ms: i64, to_ms: i64) -> Vec<Option<f64>> {
        self.recent
            .iter()
            .enumerate()
            .filter(|(i, _)| which.is_none_or(|w| w == *i))
            .map(|(_, q)| q)
            .flat_map(|q| q.iter().filter(|(ts, _)| *ts >= from_ms && *ts < to_ms).map(|(_, v)| *v))
            .collect()
    }

    fn icon_data(&self, now_ms: i64, which: Option<usize>) -> IconData {
        // columns aligned on wall-clock slices, the last one still filling
        let end = (now_ms / self.column_ms + 1) * self.column_ms;
        let columns = (0..self.columns)
            .map(|i| {
                let from = end - (self.columns - i) as i64 * self.column_ms;
                Column::from_samples(&self.pooled(which, from, from + self.column_ms))
            })
            .collect();
        let latest = Column::from_samples(&self.pooled(which, now_ms - self.current_ms, now_ms + 1));
        let current = match latest.pcts {
            Some(p) => Current::Ms(p[2]),
            None if latest.count > 0 => Current::Lost,
            None => Current::Unknown,
        };
        IconData { current, columns }
    }

    /// One line per target, for the top of the menu.
    fn status_lines(&self, which: Option<usize>) -> Vec<String> {
        self.names
            .iter()
            .zip(&self.recent)
            .enumerate()
            .map(|(i, (name, q))| {
                let last = match q.back() {
                    None => "…".to_string(),
                    Some((_, None)) => "lost".to_string(),
                    Some((_, Some(ms))) => format!("{ms:.0} ms"),
                };
                let lost = q.iter().filter(|(_, v)| v.is_none()).count();
                let loss = if q.is_empty() { 0.0 } else { 100.0 * lost as f64 / q.len() as f64 };
                let mark = if which.is_none_or(|w| w == i) { "● " } else { "○ " };
                let http = match self.last_http[i] {
                    None => String::new(),
                    Some(None) => ", HTTP failed".to_string(),
                    Some(Some(ms)) => format!(", HTTP {ms:.0} ms"),
                };
                format!("{mark}{name}: ping {last}{http}, loss {loss:.1} % (5 min)")
            })
            .collect()
    }

    fn tooltip(&self, cfg: &Config, which: Option<usize>) -> String {
        let mut lines: Vec<String> = self
            .names
            .iter()
            .zip(&self.recent)
            .enumerate()
            .map(|(i, (name, q))| {
                let last = match q.back() {
                    None => "…".to_string(),
                    Some((_, None)) => "lost".to_string(),
                    Some((_, Some(ms))) => format!("{ms:.1} ms"),
                };
                let lost = q.iter().filter(|(_, v)| v.is_none()).count();
                let loss = if q.is_empty() { 0.0 } else { 100.0 * lost as f64 / q.len() as f64 };
                let mark = if which.is_none_or(|w| w == i) { "▸ " } else { "   " };
                format!("{mark}{name}: {last}, loss {loss:.0}% over 5 min")
            })
            .collect();
        lines.push(String::new());
        lines.push(format!(
            "{}Number: median ms right now. Graph: last {} min, one column per {} s, top = {} ms,\n\
             grey = spread, tick = median, tick colour = loss \
             (green 0, cyan/blue a little, purple, orange, red heavy).",
            if which.is_none() { "All targets pooled (▸). " } else { "" },
            (self.columns as f64 * cfg.icon_column_secs / 60.0).round(),
            cfg.icon_column_secs,
            icon::ceiling_ms(&self.icon_data(crate::pinger::now_ms(), which).columns)
        ));
        lines.join("\n")
    }
}

struct TrayFeed {
    handle: TrayHandle,
    sink: Option<PngSink>,
    base: TrayState,
    thresholds: Thresholds,
    height: usize,
    last: Option<Instant>,
}

impl TrayFeed {
    async fn refresh(&mut self, live: &Live, cfg: &Config) {
        if self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(900)) {
            return;
        }
        self.last = Some(Instant::now());
        let which = crate::tray::selected_target(&self.base.selection);
        let data = live.icon_data(pinger::now_ms(), which);
        let (w, h, rgba) = icon::render(&data, self.height, self.thresholds);
        let icon_file = self.sink.as_mut().and_then(|s| {
            s.write(w, h, &rgba).map_err(|e| tracing::warn!("writing icon: {e}")).ok()
        });
        let state = TrayState {
            icon_file,
            pixmap: (w, h, icon::rgba_to_argb(rgba)),
            tooltip: live.tooltip(cfg, which),
            status: live.status_lines(which),
            ..self.base.clone()
        };
        self.handle.update(state).await;
    }
}

/// Set in the environment of a collector restarted for new settings.
pub const RESTARTED: &str = "PINGTRAY_RESTARTED";

fn modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Restarts this process (same arguments) when the config file changes:
/// simpler and safer than reconfiguring pingers, endpoint and tray live.
fn watch_config(path: PathBuf) {
    std::thread::spawn(move || {
        let start = modified(&path);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let now = modified(&path);
            if now.is_some() && now != start {
                // let the writer finish
                std::thread::sleep(Duration::from_millis(300));
                if let Err(e) = Config::load(&path) {
                    tracing::error!("new settings not applied: {e}");
                    continue;
                }
                tracing::info!("settings changed, restarting");
                restart();
            }
        }
    });
}

fn restart() {
    let Ok(exe) = crate::tray::own_exe() else { return };
    // a new process, not exec(): the tray is registered on D-Bus under a name
    // containing the PID, and reusing it confuses the panel
    let started = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(RESTARTED, "1")
        .spawn();
    match started {
        // the new one waits for our lock, released on exit
        Ok(_) => std::process::exit(0),
        Err(e) => tracing::error!("restart failed: {e}"),
    }
}

/// Phone positions: HTTP endpoint and watched folder, when configured.
fn start_location_inputs(cfg: &Config) {
    let loc = &cfg.location;
    if let Some(listen) = &loc.listen {
        let started = listen
            .parse()
            .map_err(anyhow::Error::from)
            .and_then(|addr| crate::location::spawn_server(addr, loc.token.clone(), config::db_path()));
        if let Err(e) = started {
            tracing::error!("location endpoint: {e}");
        }
    }
    if let Some(dir) = loc.watch_dir() {
        tracing::info!("watching {} for position files", dir.display());
        crate::location::spawn_dir_watcher(dir, config::db_path());
    }
}

pub async fn run(cfg: Config, config_path: PathBuf, with_tray: bool) -> Result<()> {
    let cfg = Arc::new(cfg);
    if cfg.targets.is_empty() {
        anyhow::bail!("no [[target]] in {}", config_path.display());
    }
    let mut store = Store::open(&config::db_path())?;
    let ids = cfg
        .targets
        .iter()
        .map(|t| store.upsert_target(&t.name, &t.host, Kind::Icmp.as_str()))
        .collect::<Result<Vec<_>>>()?;
    let http_ids = cfg
        .targets
        .iter()
        .map(|t| t.http.as_ref().map(|u| store.upsert_target(&t.name, u, Kind::Http.as_str())).transpose())
        .collect::<Result<Vec<_>>>()?;

    start_location_inputs(&cfg);
    watch_config(config_path.clone());

    let (tx, mut rx) = mpsc::channel::<Sample>(256);
    pinger::spawn_http(cfg.clone(), tx.clone());
    pinger::spawn_all(cfg.clone(), tx)?;

    let mut live = Live::new(&cfg);
    // start with a full icon after a restart
    let since = pinger::now_ms() - live.keep_ms;
    for (idx, id) in ids.iter().enumerate() {
        for (ts_ms, rtt) in store.samples_since(*id, since)? {
            live.recent[idx].push_back((ts_ms, rtt));
        }
    }
    let mut tray = None;
    if with_tray {
        let choices: Vec<String> = cfg.targets.iter().map(|t| t.name.clone()).collect();
        let selection = crate::tray::load_selection(&choices, cfg.tray_target.as_deref());
        let base = TrayState {
            tooltip: "starting…".into(),
            config_path: config_path.clone(),
            choices,
            selection,
            ..Default::default()
        };
        if let Some(handle) = TrayHandle::spawn(base.clone()).await {
            let sink = PngSink::new(&config::runtime_dir().join("icons"))
                .map_err(|e| tracing::warn!("icon directory: {e}"))
                .ok();
            let thresholds = Thresholds { warn_ms: cfg.warn_ms, bad_ms: cfg.bad_ms };
            tray = Some(TrayFeed { handle, sink, base, thresholds, height: cfg.icon_height, last: None });
        }
    }

    let retention_ms = cfg.retention_days as i64 * 86_400_000;
    let mut pending = Vec::new();
    let mut pending_http = Vec::new();
    let mut last_flush = Instant::now();
    let mut last_purge: Option<Instant> = None;

    while let Some(s) = rx.recv().await {
        let us = |d: Duration| d.as_micros().min(u32::MAX as u128) as u32;
        let rtt_us = s.rtt.map(us);
        match s.kind {
            Kind::Icmp => {
                live.push(&s);
                pending.push((ids[s.target], s.ts_ms, rtt_us));
            }
            Kind::Http => {
                live.push_http(&s);
                let Some(id) = http_ids[s.target] else { continue };
                pending.push((id, s.ts_ms, rtt_us));
                if let Some(p) = s.http {
                    pending_http.push((id, s.ts_ms, [us(p.dns), us(p.connect), us(p.tls), us(p.wait)], p.status));
                }
            }
        }

        if last_flush.elapsed() >= cfg.interval().mul_f64(0.9) {
            last_flush = Instant::now();
            let batch = std::mem::take(&mut pending);
            let details = std::mem::take(&mut pending_http);
            tokio::task::block_in_place(|| {
                store.insert_samples(&batch)?;
                store.insert_http_details(&details)
            })
            .unwrap_or_else(|e| tracing::error!("storing samples: {e}"));
            if last_purge.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
                last_purge = Some(Instant::now());
                let cutoff = pinger::now_ms() - retention_ms;
                match tokio::task::block_in_place(|| store.purge_before(cutoff)) {
                    Ok(n) if n > 0 => tracing::info!("purged {n} old samples"),
                    Ok(_) => {}
                    Err(e) => tracing::error!("purging: {e}"),
                }
            }
        }
        if let Some(tray) = &mut tray {
            tray.refresh(&live, &cfg).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;

    #[test]
    fn columns_pool_targets_and_current_is_median() {
        let mut cfg = Config::default();
        cfg.icon_columns = 2;
        cfg.icon_column_secs = 10.0;
        cfg.targets = vec![
            Target { name: "a".into(), host: "a".into(), http: None },
            Target { name: "b".into(), host: "b".into(), http: None },
        ];
        let mut live = Live::new(&cfg);
        let ms = |v: u64| Some(Duration::from_millis(v));
        for (target, ts_ms, rtt) in [(0, 1_000, ms(10)), (1, 1_000, None), (0, 12_000, ms(20)), (1, 12_000, ms(40))] {
            live.push(&Sample { target, ts_ms, rtt, http: None, kind: Kind::Icmp });
        }
        let d = live.icon_data(13_000, None);
        assert_eq!(d.columns[0].loss, 0.5);
        assert_eq!(d.columns[1].pcts.unwrap()[2], 30.0);
        assert_eq!(d.current, Current::Ms(30.0));
    }
}
