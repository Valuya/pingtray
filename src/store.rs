//! SQLite history. One writer (the collector), any number of readers (viewers).

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub struct Store {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct TargetRow {
    pub id: i64,
    pub name: String,
    pub host: String,
}

/// Percentiles kept per bucket, SmokePing style.
pub const PCTS: [f64; 7] = [0.0, 0.10, 0.25, 0.50, 0.75, 0.90, 1.0];
pub const MEDIAN: usize = 3;

/// Aggregate of the samples falling in one time bucket.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    pub ts_ms: i64,
    /// RTT at each of [`PCTS`], in ms; `None` when every ping was lost.
    pub pcts: Option<[f64; 7]>,
    pub count: u32,
    pub lost: u32,
}

impl Bucket {
    pub fn loss_pct(&self) -> f64 {
        if self.count == 0 { 0.0 } else { 100.0 * self.lost as f64 / self.count as f64 }
    }
}

/// Bump when adding a migration step below.
const SCHEMA_VERSION: i64 = 1;

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self.conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS target (
                     id   INTEGER PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE,
                     host TEXT NOT NULL
                 );
                 -- rtt_us NULL means lost (timeout, unreachable, DNS failure).
                 CREATE TABLE IF NOT EXISTS sample (
                     target_id INTEGER NOT NULL,
                     ts_ms     INTEGER NOT NULL,
                     rtt_us    INTEGER,
                     PRIMARY KEY (target_id, ts_ms)
                 ) WITHOUT ROWID;",
            )?;
        }
        if version < SCHEMA_VERSION {
            self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Returns the id of the target with this name, creating or updating it.
    pub fn upsert_target(&self, name: &str, host: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO target(name, host) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET host = excluded.host",
            params![name, host],
        )?;
        Ok(self.conn.query_row("SELECT id FROM target WHERE name = ?1", [name], |r| r.get(0))?)
    }

    pub fn targets(&self) -> Result<Vec<TargetRow>> {
        let mut stmt = self.conn.prepare("SELECT id, name, host FROM target ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(TargetRow { id: r.get(0)?, name: r.get(1)?, host: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn insert_samples(&mut self, samples: &[(i64, i64, Option<u32>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO sample(target_id, ts_ms, rtt_us) VALUES (?1, ?2, ?3)",
            )?;
            for (target_id, ts_ms, rtt_us) in samples {
                stmt.execute(params![target_id, ts_ms, rtt_us])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Raw samples of one target since `from_ms`, oldest first; RTT in ms.
    pub fn samples_since(&self, target_id: i64, from_ms: i64) -> Result<Vec<(i64, Option<f64>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts_ms, rtt_us FROM sample WHERE target_id = ?1 AND ts_ms >= ?2 ORDER BY ts_ms",
        )?;
        let rows = stmt.query_map(params![target_id, from_ms], |r| {
            Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.map(|us| us as f64 / 1000.0)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn purge_before(&self, ts_ms: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM sample WHERE ts_ms < ?1", [ts_ms])?)
    }

    pub fn first_sample_ms(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MIN(ts_ms) FROM sample", [], |r| r.get::<_, Option<i64>>(0))
            .optional()?
            .flatten())
    }

    /// Samples of one target in `[from_ms, to_ms)`, aggregated into buckets of `bucket_ms`.
    pub fn buckets(&self, target_id: i64, from_ms: i64, to_ms: i64, bucket_ms: i64) -> Result<Vec<Bucket>> {
        let bucket_ms = bucket_ms.max(1);
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts_ms, rtt_us FROM sample
             WHERE target_id = ?1 AND ts_ms >= ?2 AND ts_ms < ?3 ORDER BY ts_ms",
        )?;
        let mut rows = stmt.query(params![target_id, from_ms, to_ms])?;
        let mut out = Vec::new();
        let mut cur: Option<i64> = None;
        let mut rtts: Vec<f64> = Vec::new();
        let mut count = 0u32;
        let flush = |out: &mut Vec<Bucket>, ts_ms: i64, rtts: &mut Vec<f64>, count: u32| {
            rtts.sort_by(f64::total_cmp);
            let pcts = (!rtts.is_empty()).then(|| PCTS.map(|p| crate::icon::percentile(rtts, p)));
            out.push(Bucket { ts_ms, pcts, count, lost: count - rtts.len() as u32 });
            rtts.clear();
        };
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let b = ts.div_euclid(bucket_ms) * bucket_ms;
            if cur != Some(b) {
                if let Some(c) = cur {
                    flush(&mut out, c, &mut rtts, count);
                }
                cur = Some(b);
                count = 0;
            }
            count += 1;
            if let Some(us) = r.get::<_, Option<i64>>(1)? {
                rtts.push(us as f64 / 1000.0);
            }
        }
        if let Some(c) = cur {
            flush(&mut out, c, &mut rtts, count);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_aggregate_rtt_and_loss() {
        let dir = std::env::temp_dir().join(format!("pingtray-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let mut store = Store::open(&path).unwrap();
        let id = store.upsert_target("a", "1.2.3.4").unwrap();
        assert_eq!(store.upsert_target("a", "5.6.7.8").unwrap(), id);
        store
            .insert_samples(&[(id, 1000, Some(10_000)), (id, 2000, Some(30_000)), (id, 3000, None), (id, 11_000, None)])
            .unwrap();
        let b = store.buckets(id, 0, 20_000, 10_000).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].pcts.unwrap()[MEDIAN], 20.0);
        assert_eq!((b[0].count, b[0].lost), (3, 1));
        assert_eq!(b[1].pcts, None);
        assert_eq!(b[1].loss_pct(), 100.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
