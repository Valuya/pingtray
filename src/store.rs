//! SQLite history. One writer (the collector), any number of readers (viewers).

use std::path::Path;
use crate::location::Fix;

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
    /// "icmp" (ping) or "http".
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub id: i64,
    pub ts_ms: i64,
    pub text: String,
}

/// Sources differ wildly (phone GPS: 5 m, Wi-Fi: 50 m, IP guess: 25 km):
/// drop a fix when a much more accurate one exists around the same time, so
/// that a coarse guess does not tear a precise track apart.
pub fn drop_vague(fixes: Vec<Fix>) -> Vec<Fix> {
    const AROUND_MS: i64 = 120_000;
    let acc = |f: &Fix| f.acc_m.unwrap_or(crate::location::DEFAULT_ACC_M);
    let keep: Vec<bool> = (0..fixes.len())
        .map(|i| {
            let f = &fixes[i];
            let lo = fixes.partition_point(|g| g.ts_ms < f.ts_ms - AROUND_MS);
            let hi = fixes.partition_point(|g| g.ts_ms <= f.ts_ms + AROUND_MS);
            acc(f) <= crate::geoclue::USELESS_M && !fixes[lo..hi].iter().any(|g| acc(g) * 4.0 < acc(f))
        })
        .collect();
    fixes.into_iter().zip(keep).filter_map(|(f, k)| k.then_some(f)).collect()
}

/// Percentiles kept per bucket, SmokePing style.
/// Every 5 %, so that the smoke can fade smoothly.
pub const PCTS: [f64; 21] = [
    0.0, 0.05, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.45, 0.50, 0.55, 0.60, 0.65, 0.70, 0.75, 0.80,
    0.85, 0.90, 0.95, 1.0,
];
pub const MIN: usize = 0;
pub const P10: usize = 2;
pub const MEDIAN: usize = 10;
pub const P90: usize = 18;
pub const MAX: usize = 20;

/// Aggregate of the samples falling in one time bucket.
#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    pub ts_ms: i64,
    /// RTT at each of [`PCTS`], in ms; `None` when every ping was lost.
    pub pcts: Option<[f64; 21]>,
    pub count: u32,
    pub lost: u32,
}

impl Bucket {
    #[cfg(test)]
    pub fn loss_pct(&self) -> f64 {
        if self.count == 0 { 0.0 } else { 100.0 * self.lost as f64 / self.count as f64 }
    }
}

/// Bump when adding a migration step below.
const SCHEMA_VERSION: i64 = 6;

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
        if version < 2 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS location (
                     ts_ms    INTEGER NOT NULL,
                     source   TEXT NOT NULL,
                     lat      REAL NOT NULL,
                     lon      REAL NOT NULL,
                     acc_m    REAL,
                     alt_m    REAL,
                     speed_ms REAL,
                     PRIMARY KEY (ts_ms, source)
                 ) WITHOUT ROWID;
                 -- files already imported from the watched folder, by size and mtime
                 CREATE TABLE IF NOT EXISTS imported_file (
                     path  TEXT PRIMARY KEY,
                     stamp TEXT NOT NULL
                 );
                 -- OpenStreetMap railways, per tile, for the transport guess
                 CREATE TABLE IF NOT EXISTS rail_tile (
                     z INTEGER NOT NULL, x INTEGER NOT NULL, y INTEGER NOT NULL,
                     fetched_ms INTEGER NOT NULL,
                     ways TEXT NOT NULL,
                     PRIMARY KEY (z, x, y)
                 );",
            )?;
        }
        if version < 3 {
            // a destination may now have two series: ping and HTTP
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE target_v3 (
                     id   INTEGER PRIMARY KEY,
                     name TEXT NOT NULL,
                     host TEXT NOT NULL,
                     kind TEXT NOT NULL DEFAULT 'icmp',
                     UNIQUE (name, kind)
                 );
                 INSERT INTO target_v3(id, name, host, kind) SELECT id, name, host, 'icmp' FROM target;
                 DROP TABLE target;
                 ALTER TABLE target_v3 RENAME TO target;
                 -- phases of HTTP probes; the total is in sample.rtt_us
                 CREATE TABLE IF NOT EXISTS http_detail (
                     target_id  INTEGER NOT NULL,
                     ts_ms      INTEGER NOT NULL,
                     dns_us     INTEGER NOT NULL,
                     connect_us INTEGER NOT NULL,
                     tls_us     INTEGER NOT NULL,
                     wait_us    INTEGER NOT NULL,
                     status     INTEGER NOT NULL,
                     PRIMARY KEY (target_id, ts_ms)
                 ) WITHOUT ROWID;
                 COMMIT;",
            )?;
        }
        if version < 4 {
            self.conn.execute_batch(
                "-- what the graph cannot know: 'crowded train', 'Wi-Fi on board'...
                 CREATE TABLE IF NOT EXISTS note (
                     id    INTEGER PRIMARY KEY,
                     ts_ms INTEGER NOT NULL,
                     text  TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS note_ts ON note(ts_ms);",
            )?;
        }
        if version < 5 {
            self.conn.execute_batch(
                "-- the latest guess too coarse to record (GeoClue's IP fallback: 25 km),
                 -- so that an empty map can still show roughly where we are
                 CREATE TABLE IF NOT EXISTS rough_position (
                     id    INTEGER PRIMARY KEY CHECK (id = 1),
                     ts_ms INTEGER NOT NULL,
                     lat   REAL NOT NULL,
                     lon   REAL NOT NULL,
                     acc_m REAL
                 );",
            )?;
        }
        if version < 6 {
            self.conn.execute_batch(
                "-- this computer's load every few seconds, for the system window;
                 -- shares in %, sizes in bytes, rates in bytes/s, cores 1 byte each (half %)
                 CREATE TABLE IF NOT EXISTS system_sample (
                     ts_ms         INTEGER PRIMARY KEY,
                     cpu_user      REAL NOT NULL,
                     cpu_system    REAL NOT NULL,
                     cpu_iowait    REAL NOT NULL,
                     cpu_steal     REAL NOT NULL,
                     cores         BLOB NOT NULL,
                     mem_total     REAL NOT NULL,
                     mem_available REAL NOT NULL,
                     mem_cache     REAL NOT NULL,
                     swap_total    REAL NOT NULL,
                     swap_zram     REAL NOT NULL,
                     swap_disk     REAL NOT NULL,
                     zram_ram      REAL NOT NULL,
                     net_in        REAL NOT NULL,
                     net_out       REAL NOT NULL,
                     disk_read     REAL NOT NULL,
                     disk_write    REAL NOT NULL,
                     psi_cpu       REAL NOT NULL,
                     psi_mem       REAL NOT NULL,
                     psi_io        REAL NOT NULL
                 );",
            )?;
        }
        if version < SCHEMA_VERSION {
            self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    /// Returns the id of the target with this name, creating or updating it.
    pub fn upsert_target(&self, name: &str, host: &str, kind: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO target(name, host, kind) VALUES (?1, ?2, ?3)
             ON CONFLICT(name, kind) DO UPDATE SET host = excluded.host",
            params![name, host, kind],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM target WHERE name = ?1 AND kind = ?2", [name, kind], |r| r.get(0))?)
    }

    pub fn targets(&self) -> Result<Vec<TargetRow>> {
        let mut stmt = self.conn.prepare("SELECT id, name, host, kind FROM target ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(TargetRow { id: r.get(0)?, name: r.get(1)?, host: r.get(2)?, kind: r.get(3)? })
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

    /// Raw samples of one target in `[from_ms, to_ms)`, oldest first; RTT in ms.
    pub fn samples_between(&self, target_id: i64, from_ms: i64, to_ms: i64) -> Result<Vec<(i64, Option<f64>)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts_ms, rtt_us FROM sample WHERE target_id = ?1 AND ts_ms >= ?2 AND ts_ms < ?3 ORDER BY ts_ms",
        )?;
        let rows = stmt.query_map(params![target_id, from_ms, to_ms], |r| {
            Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.map(|us| us as f64 / 1000.0)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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


    /// Inserts positions; returns how many were new.
    pub fn insert_fixes(&mut self, fixes: &[Fix], source: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut added = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO location(ts_ms, source, lat, lon, acc_m, alt_m, speed_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for f in fixes {
                added += stmt.execute(params![f.ts_ms, source, f.lat, f.lon, f.acc_m, f.alt_m, f.speed_ms])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Positions in `[from_ms, to_ms)`, sorted by time; when several sources
    /// report the same instant, the most accurate wins.
    pub fn fixes(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Fix>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts_ms, lat, lon, acc_m, alt_m, speed_ms FROM location
             WHERE ts_ms >= ?1 AND ts_ms < ?2 ORDER BY ts_ms, COALESCE(acc_m, 1e9)",
        )?;
        let rows = stmt.query_map(params![from_ms, to_ms], |r| {
            Ok(Fix {
                ts_ms: r.get(0)?,
                lat: r.get(1)?,
                lon: r.get(2)?,
                acc_m: r.get(3)?,
                alt_m: r.get(4)?,
                speed_ms: r.get(5)?,
            })
        })?;
        let mut out: Vec<Fix> = rows.collect::<Result<_, _>>()?;
        out.dedup_by_key(|f| f.ts_ms);
        Ok(drop_vague(out))
    }

    /// Time of the latest position from any source but `source`.
    pub fn last_fix_ms_except(&self, source: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts_ms FROM location WHERE source != ?1 ORDER BY ts_ms DESC LIMIT 1",
                params![source],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn insert_system(&self, d: &crate::system::Detail) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO system_sample VALUES
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            )?
            .execute(params![
                d.ts_ms, d.cpu_user, d.cpu_system, d.cpu_iowait, d.cpu_steal, d.cores_blob(), d.mem_total,
                d.mem_available, d.mem_cache, d.swap_total, d.swap_zram, d.swap_disk, d.zram_ram, d.net_in,
                d.net_out, d.disk_read, d.disk_write, d.psi_cpu, d.psi_mem, d.psi_io
            ])?;
        Ok(())
    }

    /// System samples in `[from_ms, to_ms)`, by time.
    pub fn system(&self, from_ms: i64, to_ms: i64) -> Result<Vec<crate::system::Detail>> {
        let mut stmt = self.conn.prepare_cached("SELECT * FROM system_sample WHERE ts_ms >= ?1 AND ts_ms < ?2 ORDER BY ts_ms")?;
        let rows = stmt.query_map(params![from_ms, to_ms], |r| {
            let blob: Vec<u8> = r.get(5)?;
            Ok(crate::system::Detail {
                ts_ms: r.get(0)?,
                cpu_user: r.get(1)?,
                cpu_system: r.get(2)?,
                cpu_iowait: r.get(3)?,
                cpu_steal: r.get(4)?,
                cores: crate::system::Detail::cores_from_blob(&blob),
                mem_total: r.get(6)?,
                mem_available: r.get(7)?,
                mem_cache: r.get(8)?,
                swap_total: r.get(9)?,
                swap_zram: r.get(10)?,
                swap_disk: r.get(11)?,
                zram_ram: r.get(12)?,
                net_in: r.get(13)?,
                net_out: r.get(14)?,
                disk_read: r.get(15)?,
                disk_write: r.get(16)?,
                psi_cpu: r.get(17)?,
                psi_mem: r.get(18)?,
                psi_io: r.get(19)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn prune_system(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM system_sample WHERE ts_ms < ?1", [before_ms])?)
    }

    pub fn set_rough_position(&self, f: &Fix) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO rough_position(id, ts_ms, lat, lon, acc_m) VALUES (1, ?1, ?2, ?3, ?4)",
            params![f.ts_ms, f.lat, f.lon, f.acc_m],
        )?;
        Ok(())
    }

    /// The latest coarse guess, or the latest recorded position if newer.
    pub fn approximate_position(&self) -> Result<Option<Fix>> {
        let row = |r: &rusqlite::Row<'_>| {
            Ok(Fix { ts_ms: r.get(0)?, lat: r.get(1)?, lon: r.get(2)?, acc_m: r.get(3)?, alt_m: None, speed_ms: None })
        };
        let rough = self.conn.query_row("SELECT ts_ms, lat, lon, acc_m FROM rough_position", [], row).optional()?;
        let last = self
            .conn
            .query_row("SELECT ts_ms, lat, lon, acc_m FROM location ORDER BY ts_ms DESC LIMIT 1", [], row)
            .optional()?;
        Ok([rough, last].into_iter().flatten().max_by_key(|f| f.ts_ms))
    }

    pub fn first_fix_ms(&self) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT MIN(ts_ms) FROM location", [], |r| r.get(0))?)
    }

    pub fn has_fixes(&self) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM location)", [], |r| r.get(0))?)
    }

    pub fn file_stamp(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT stamp FROM imported_file WHERE path = ?1", [path], |r| r.get(0))
            .optional()?)
    }

    pub fn set_file_stamp(&self, path: &str, stamp: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO imported_file(path, stamp) VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET stamp = excluded.stamp",
            params![path, stamp],
        )?;
        Ok(())
    }

    /// Cached railway geometry of a tile (JSON) and when it was fetched.
    pub fn rail_tile(&self, z: u32, x: u32, y: u32) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT fetched_ms, ways FROM rail_tile WHERE z = ?1 AND x = ?2 AND y = ?3",
                params![z, x, y],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_rail_tile(&self, z: u32, x: u32, y: u32, fetched_ms: i64, ways: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO rail_tile(z, x, y, fetched_ms, ways) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![z, x, y, fetched_ms, ways],
        )?;
        Ok(())
    }
    /// (target_id, ts_ms, dns, connect, tls, wait in µs, HTTP status)
    pub fn insert_http_details(&mut self, rows: &[(i64, i64, [u32; 4], u16)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO http_detail(target_id, ts_ms, dns_us, connect_us, tls_us, wait_us, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (id, ts, p, status) in rows {
                stmt.execute(params![id, ts, p[0], p[1], p[2], p[3], status])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Median DNS, connect, TLS and wait times (ms) per bucket of an HTTP series.
    pub fn http_phases(&self, target_id: i64, from_ms: i64, to_ms: i64, bucket_ms: i64) -> Result<Vec<(i64, [f64; 4])>> {
        let bucket_ms = bucket_ms.max(1);
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts_ms, dns_us, connect_us, tls_us, wait_us FROM http_detail
             WHERE target_id = ?1 AND ts_ms >= ?2 AND ts_ms < ?3 ORDER BY ts_ms",
        )?;
        let mut rows = stmt.query(params![target_id, from_ms, to_ms])?;
        let mut out = Vec::new();
        let mut cur: Option<i64> = None;
        let mut acc: [Vec<f64>; 4] = Default::default();
        let flush = |out: &mut Vec<(i64, [f64; 4])>, b: i64, acc: &mut [Vec<f64>; 4]| {
            let med = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                let m = crate::icon::percentile(v, 0.5);
                v.clear();
                m
            };
            let p = [med(&mut acc[0]), med(&mut acc[1]), med(&mut acc[2]), med(&mut acc[3])];
            out.push((b, p));
        };
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let b = ts.div_euclid(bucket_ms) * bucket_ms;
            if cur.is_some_and(|c| c != b) {
                flush(&mut out, cur.unwrap(), &mut acc);
            }
            cur = Some(b);
            for (k, v) in acc.iter_mut().enumerate() {
                v.push(r.get::<_, i64>(k + 1)? as f64 / 1000.0);
            }
        }
        if let Some(c) = cur {
            flush(&mut out, c, &mut acc);
        }
        Ok(out)
    }

    pub fn add_note(&self, ts_ms: i64, text: &str) -> Result<i64> {
        self.conn.execute("INSERT INTO note(ts_ms, text) VALUES (?1, ?2)", params![ts_ms, text])?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_note(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM note WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Notes in `[from_ms, to_ms)`, oldest first: (id, ts_ms, text).
    pub fn notes(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, ts_ms, text FROM note WHERE ts_ms >= ?1 AND ts_ms < ?2 ORDER BY ts_ms",
        )?;
        let rows = stmt.query_map(params![from_ms, to_ms], |r| Ok(Note { id: r.get(0)?, ts_ms: r.get(1)?, text: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn recent_notes(&self, limit: usize) -> Result<Vec<Note>> {
        let mut stmt = self.conn.prepare_cached("SELECT id, ts_ms, text FROM note ORDER BY ts_ms DESC LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |r| Ok(Note { id: r.get(0)?, ts_ms: r.get(1)?, text: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Distinct texts, most used first, for one-click reuse.
    pub fn frequent_notes(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT text FROM note GROUP BY text ORDER BY COUNT(*) DESC, MAX(ts_ms) DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn purge_before(&self, ts_ms: i64) -> Result<usize> {
        self.conn.execute("DELETE FROM http_detail WHERE ts_ms < ?1", [ts_ms])?;
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
    fn system_and_rough_round_trip() {
        let dir = std::env::temp_dir().join(format!("pingtray-test-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let d = crate::system::Detail { ts_ms: 5000, cpu_user: 12.5, cores: vec![10.0, 99.5], mem_total: 8e9, psi_io: 0.3, ..Default::default() };
        store.insert_system(&d).unwrap();
        store.insert_system(&crate::system::Detail { ts_ms: 1000, ..Default::default() }).unwrap();
        assert_eq!(store.system(2000, 9000).unwrap(), vec![d]);
        assert_eq!(store.prune_system(2000).unwrap(), 1);
        let rough = Fix { ts_ms: 9000, lat: 50.8, lon: 4.3, acc_m: Some(25_000.0), alt_m: None, speed_ms: None };
        assert_eq!(store.approximate_position().unwrap(), None);
        store.set_rough_position(&rough).unwrap();
        assert_eq!(store.approximate_position().unwrap(), Some(rough));
    }

    #[test]
    fn buckets_aggregate_rtt_and_loss() {
        let dir = std::env::temp_dir().join(format!("pingtray-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite");
        let _ = std::fs::remove_file(&path);
        let mut store = Store::open(&path).unwrap();
        let id = store.upsert_target("a", "1.2.3.4", "icmp").unwrap();
        assert_eq!(store.upsert_target("a", "5.6.7.8", "icmp").unwrap(), id);
        assert_ne!(store.upsert_target("a", "https://a/", "http").unwrap(), id);
        store
            .insert_samples(&[(id, 1000, Some(10_000)), (id, 2000, Some(30_000)), (id, 3000, None), (id, 11_000, None)])
            .unwrap();
        let b = store.buckets(id, 0, 20_000, 10_000).unwrap();
        let fix = |ts_ms, acc| Fix { ts_ms, lat: 50.0, lon: 4.0, acc_m: Some(acc), alt_m: None, speed_ms: None };
        let kept = drop_vague(vec![fix(0, 25_000.0), fix(30_000, 8.0), fix(600_000, 25_000.0)]);
        // a 25 km guess goes, alone or next to a GPS fix
        assert_eq!(kept.iter().map(|f| f.ts_ms).collect::<Vec<_>>(), vec![30_000]);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].pcts.unwrap()[MEDIAN], 20.0);
        assert_eq!((b[0].count, b[0].lost), (3, 1));
        assert_eq!(b[1].pcts, None);
        assert_eq!(b[1].loss_pct(), 100.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
