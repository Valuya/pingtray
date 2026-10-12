//! Long-running process: pings, stores history, feeds the tray icon.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::{self, Config};
use crate::icon::{Bar, Thresholds};
use crate::pinger::{self, Sample};
use crate::store::Store;
use crate::tray::{TrayHandle, TrayState};

/// Recent samples per target, for the icon and the tooltip.
struct Live {
    names: Vec<String>,
    recent: Vec<VecDeque<Option<f64>>>,
    keep: usize,
}

const TOOLTIP_WINDOW: Duration = Duration::from_secs(300);

impl Live {
    fn new(cfg: &Config) -> Live {
        let window = (TOOLTIP_WINDOW.as_secs_f64() / cfg.interval_secs).ceil() as usize;
        Live {
            names: cfg.targets.iter().map(|t| t.name.clone()).collect(),
            recent: vec![VecDeque::new(); cfg.targets.len()],
            keep: window.max(cfg.icon_samples),
        }
    }

    fn push(&mut self, s: &Sample) {
        let q = &mut self.recent[s.target];
        q.push_back(s.rtt.map(|d| d.as_secs_f64() * 1000.0));
        while q.len() > self.keep {
            q.pop_front();
        }
    }

    /// Bar i is the worst result among the targets' i-th most recent pings.
    fn bars(&self, n: usize) -> Vec<Bar> {
        let mut bars: Vec<Bar> = (0..n)
            .map(|i| {
                self.recent
                    .iter()
                    .filter_map(|q| q.len().checked_sub(i + 1).map(|j| q[j]))
                    .reduce(|a, b| match (a, b) {
                        (Some(x), Some(y)) => Some(x.max(y)),
                        _ => None,
                    })
            })
            .collect();
        bars.reverse();
        bars
    }

    fn tooltip(&self) -> String {
        self.names
            .iter()
            .zip(&self.recent)
            .map(|(name, q)| {
                let last = match q.back() {
                    None => "…".to_string(),
                    Some(None) => "lost".to_string(),
                    Some(Some(ms)) => format!("{ms:.1} ms"),
                };
                let lost = q.iter().filter(|v| v.is_none()).count();
                let loss = if q.is_empty() { 0.0 } else { 100.0 * lost as f64 / q.len() as f64 };
                format!("{name}: {last} (loss {loss:.0}% / 5 min)")
            })
            .collect::<Vec<_>>()
            .join("\n")
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
    let tray = if with_tray {
        TrayHandle::spawn(TrayState {
            bars: live.bars(cfg.icon_samples),
            tooltip: "starting…".into(),
            thresholds: Thresholds { warn_ms: cfg.warn_ms, bad_ms: cfg.bad_ms },
            config_path: config_path.clone(),
        })
        .await
    } else {
        None
    };

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
        if let Some(tray) = &tray {
            tray.update(live.bars(cfg.icon_samples), live.tooltip()).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Target;

    #[test]
    fn worst_target_wins_and_loss_dominates() {
        let mut cfg = Config::default();
        cfg.targets = vec![
            Target { name: "a".into(), host: "a".into() },
            Target { name: "b".into(), host: "b".into() },
        ];
        let mut live = Live::new(&cfg);
        let ms = |v: u64| Some(Duration::from_millis(v));
        for (target, rtt) in [(0, ms(10)), (1, ms(30)), (0, ms(5)), (1, None), (0, ms(7))] {
            live.push(&Sample { target, ts_ms: 0, rtt });
        }
        let bars = live.bars(3);
        assert_eq!(bars[0], Some(Some(10.0)));
        assert_eq!(bars[1], Some(Some(30.0)));
        assert_eq!(bars[2], Some(None));
    }
}
