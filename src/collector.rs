//! Long-running process: pings, stores history, feeds the tray icon.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::{self, Config};
use crate::icon::{self, Column, Current, IconData, PngSink, Thresholds};
use crate::pinger::{self, Sample};
use crate::store::Store;
use crate::tray::{TrayHandle, TrayState};

/// Recent samples per target, for the icon and the tooltip.
struct Live {
    names: Vec<String>,
    recent: Vec<VecDeque<(i64, Option<f64>)>>,
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
            keep_ms: TOOLTIP_WINDOW_MS.max(column_ms * cfg.icon_columns as i64),
            columns: cfg.icon_columns,
            column_ms,
            current_ms: ((cfg.interval_secs * 2.5).max(5.0) * 1000.0) as i64,
        }
    }

    fn push(&mut self, s: &Sample) {
        let q = &mut self.recent[s.target];
        q.push_back((s.ts_ms, s.rtt.map(|d| d.as_secs_f64() * 1000.0)));
        while q.front().is_some_and(|(ts, _)| *ts < s.ts_ms - self.keep_ms) {
            q.pop_front();
        }
    }

    fn pooled(&self, from_ms: i64, to_ms: i64) -> Vec<Option<f64>> {
        self.recent
            .iter()
            .flat_map(|q| q.iter().filter(|(ts, _)| *ts >= from_ms && *ts < to_ms).map(|(_, v)| *v))
            .collect()
    }

    fn icon_data(&self, now_ms: i64) -> IconData {
        // columns aligned on wall-clock slices, the last one still filling
        let end = (now_ms / self.column_ms + 1) * self.column_ms;
        let columns = (0..self.columns)
            .map(|i| {
                let from = end - (self.columns - i) as i64 * self.column_ms;
                Column::from_samples(&self.pooled(from, from + self.column_ms))
            })
            .collect();
        let latest = Column::from_samples(&self.pooled(now_ms - self.current_ms, now_ms + 1));
        let current = match latest.pcts {
            Some(p) => Current::Ms(p[2]),
            None if latest.count > 0 => Current::Lost,
            None => Current::Unknown,
        };
        IconData { current, columns }
    }

    fn tooltip(&self, cfg: &Config) -> String {
        let mut lines: Vec<String> = self
            .names
            .iter()
            .zip(&self.recent)
            .map(|(name, q)| {
                let last = match q.back() {
                    None => "…".to_string(),
                    Some((_, None)) => "lost".to_string(),
                    Some((_, Some(ms))) => format!("{ms:.1} ms"),
                };
                let lost = q.iter().filter(|(_, v)| v.is_none()).count();
                let loss = if q.is_empty() { 0.0 } else { 100.0 * lost as f64 / q.len() as f64 };
                format!("{name}: {last}, loss {loss:.0}% over 5 min")
            })
            .collect();
        lines.push(String::new());
        lines.push(format!(
            "Number: median ms right now. Graph: last {} min, one column per {} s, top = {} ms,\n\
             grey = spread, tick = median, tick colour = loss \
             (green 0, cyan/blue a little, purple, orange, red heavy).",
            (self.columns as f64 * cfg.icon_column_secs / 60.0).round(),
            cfg.icon_column_secs,
            icon::ceiling_ms(&self.icon_data(crate::pinger::now_ms()).columns)
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
        let data = live.icon_data(pinger::now_ms());
        let (w, h, rgba) = icon::render(&data, self.height, self.thresholds);
        let icon_file = self.sink.as_mut().and_then(|s| {
            s.write(w, h, &rgba).map_err(|e| tracing::warn!("writing icon: {e}")).ok()
        });
        let state = TrayState {
            icon_file,
            pixmap: (w, h, icon::rgba_to_argb(rgba)),
            tooltip: live.tooltip(cfg),
            ..self.base.clone()
        };
        self.handle.update(state).await;
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
        .map(|t| store.upsert_target(&t.name, &t.host))
        .collect::<Result<Vec<_>>>()?;

    let (tx, mut rx) = mpsc::channel::<Sample>(256);
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
        let base = TrayState { tooltip: "starting…".into(), config_path: config_path.clone(), ..Default::default() };
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
    let mut last_flush = Instant::now();
    let mut last_purge: Option<Instant> = None;

    while let Some(s) = rx.recv().await {
        live.push(&s);
        let rtt_us = s.rtt.map(|d| d.as_micros().min(u32::MAX as u128) as u32);
        pending.push((ids[s.target], s.ts_ms, rtt_us));

        if last_flush.elapsed() >= cfg.interval().mul_f64(0.9) {
            last_flush = Instant::now();
            let batch = std::mem::take(&mut pending);
            tokio::task::block_in_place(|| store.insert_samples(&batch))
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
            Target { name: "a".into(), host: "a".into() },
            Target { name: "b".into(), host: "b".into() },
        ];
        let mut live = Live::new(&cfg);
        let ms = |v: u64| Some(Duration::from_millis(v));
        for (target, ts_ms, rtt) in [(0, 1_000, ms(10)), (1, 1_000, None), (0, 12_000, ms(20)), (1, 12_000, ms(40))] {
            live.push(&Sample { target, ts_ms, rtt });
        }
        let d = live.icon_data(13_000);
        assert_eq!(d.columns[0].loss, 0.5);
        assert_eq!(d.columns[1].pcts.unwrap()[2], 30.0);
        assert_eq!(d.current, Current::Ms(30.0));
    }
}
