//! SQLite storage: one connection, WAL mode, cached prepared statements.
//!
//! Two tables:
//! * `samples(ts, source, metric, value)` — numeric time series.
//! * `events(ts, severity, source, msg)`  — alerts and diagnostics.
//!
//! Nothing outside the daemon opens this file; frontends go through the socket.

use std::error::Error;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::time::Duration;

use healthd_proto::{Event, Point, Severity};
use rusqlite::{params, Connection};

/// Connection setup. Every value is a deliberate RAM or SSD-wear trade-off.
const PRAGMAS: &str = "
    -- WAL: readers (IPC) never block the writer, and a crash cannot corrupt data.
    PRAGMA journal_mode = WAL;
    -- NORMAL: in WAL mode this skips an fsync per commit. We may lose the last
    -- few seconds on power loss, which is fine for health samples and saves
    -- SSD writes (the very thing this daemon watches).
    PRAGMA synchronous = NORMAL;
    -- ~1 MiB page cache (negative = KiB) instead of the 2 MiB default: the
    -- whole working set is tiny and idle RAM is a design goal.
    PRAGMA cache_size = -1024;
    -- Auto-checkpoint after ~256 pages (~1 MiB) and cap the WAL file at 1 MiB
    -- so it cannot grow unnoticed.
    PRAGMA wal_autocheckpoint = 256;
    PRAGMA journal_size_limit = 1048576;
";

/// Table definitions.
///
/// WHY `WITHOUT ROWID` with PRIMARY KEY (source, metric, ts): the key *is* the
/// access path for `history`, so no second index is stored, and re-inserting
/// the same second simply replaces the row.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS samples (
        ts     INTEGER NOT NULL,
        source TEXT    NOT NULL,
        metric TEXT    NOT NULL,
        value  REAL    NOT NULL,
        PRIMARY KEY (source, metric, ts)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS events (
        ts       INTEGER NOT NULL,
        severity TEXT    NOT NULL,
        source   TEXT    NOT NULL,
        msg      TEXT    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS events_ts ON events (ts);
";

/// Width of a downsampling bucket in seconds (one hour).
///
/// WHY 1 h: fine enough to see a daily pattern, coarse enough to shrink a
/// month of 1-minute data 60-fold.
const BUCKET_SECS: i64 = 3_600;

/// The single SQLite connection plus its operations.
pub struct Db {
    conn: Connection,
}

impl Db {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        if let Some(dir) = path.parent() {
            // 0700: health data is nobody else's business.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        Ok(Self::init(Connection::open(path)?)?)
    }

    /// Open a throwaway in-memory database (tests only).
    #[cfg(test)]
    pub fn open_in_memory() -> Self {
        Self::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    /// Apply pragmas and schema to a fresh connection.
    fn init(conn: Connection) -> rusqlite::Result<Self> {
        // WHY 2 s: only matters if someone inspects the file with the sqlite3
        // CLI while we write; better to wait briefly than to fail a sample.
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Insert one batch of samples in a single transaction (one WAL commit).
    pub fn insert_samples(
        &mut self,
        ts: i64,
        source: &str,
        readings: &[(&str, f64)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO samples (ts, source, metric, value) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for &(metric, value) in readings {
                // NaN/inf would be stored as NULL and violate NOT NULL.
                if value.is_finite() {
                    stmt.execute(params![ts, source, metric, value])?;
                }
            }
        }
        tx.commit()
    }

    /// Insert one event.
    pub fn insert_event(
        &self,
        ts: i64,
        severity: Severity,
        source: &str,
        msg: &str,
    ) -> rusqlite::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO events (ts, severity, source, msg) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![ts, severity.as_str(), source, msg])?;
        Ok(())
    }

    /// The newest `limit` samples of one metric since `since`, oldest first.
    pub fn history(
        &self,
        source: &str,
        metric: &str,
        since: i64,
        limit: u32,
    ) -> rusqlite::Result<Vec<Point>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, value FROM (
                 SELECT ts, value FROM samples
                 WHERE source = ?1 AND metric = ?2 AND ts >= ?3
                 ORDER BY ts DESC LIMIT ?4
             ) ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![source, metric, since, limit], |r| {
            Ok(Point {
                ts: r.get(0)?,
                value: r.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// The newest `limit` events since `since` at or above `min`, oldest first.
    pub fn events(
        &self,
        since: i64,
        limit: u32,
        min: Option<Severity>,
    ) -> rusqlite::Result<Vec<Event>> {
        // Severity is stored as text; rank it in SQL so LIMIT applies after filtering.
        let min_rank = min.map_or(0_i64, |s| s as i64);
        let mut stmt = self.conn.prepare_cached(
            "SELECT ts, severity, source, msg FROM (
                 SELECT ts, severity, source, msg FROM events
                 WHERE ts >= ?1
                   AND CASE severity WHEN 'info' THEN 0 WHEN 'warn' THEN 1 ELSE 2 END >= ?2
                 ORDER BY ts DESC LIMIT ?3
             ) ORDER BY ts",
        )?;
        let rows = stmt.query_map(params![since, min_rank, limit], |r| {
            let sev: String = r.get(1)?;
            Ok(Event {
                ts: r.get(0)?,
                severity: Severity::parse(&sev).unwrap_or(Severity::Info),
                source: r.get(2)?,
                msg: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Enforce retention and downsample old samples.
    ///
    /// * Samples older than `raw_secs` collapse into one averaged row per
    ///   (source, metric, hour), stored at the hour's start timestamp.
    /// * Everything older than `keep_secs` is deleted.
    ///
    /// Runs in one transaction and is idempotent: a bucket that already holds
    /// a single hour-aligned row is left untouched.
    pub fn maintain(&mut self, now: i64, keep_secs: i64, raw_secs: i64) -> rusqlite::Result<()> {
        // WHY floor to the hour: every bucket below the cutoff is then
        // complete, so a bucket is never averaged twice with a partial hour.
        let raw_cutoff = (now - raw_secs).div_euclid(BUCKET_SECS) * BUCKET_SECS;
        let keep_cutoff = now - keep_secs;

        // Maintenance runs every few hours, so plain `execute` (no statement
        // cache) is fine and keeps the cache for the hot-path statements.
        let tx = self.conn.transaction()?;
        // The bucket width is bound as ?2 (not written into the SQL) so it can
        // only ever come from BUCKET_SECS.
        // HAVING MAX(ts % ?2) > 0: only touch buckets holding at least one
        // not-yet-aligned row.
        tx.execute(
            "INSERT OR REPLACE INTO samples (ts, source, metric, value)
             SELECT (ts / ?2) * ?2, source, metric, AVG(value)
             FROM samples WHERE ts < ?1
             GROUP BY source, metric, ts / ?2
             HAVING MAX(ts % ?2) > 0",
            params![raw_cutoff, BUCKET_SECS],
        )?;
        tx.execute(
            "DELETE FROM samples WHERE ts < ?1 AND ts % ?2 != 0",
            params![raw_cutoff, BUCKET_SECS],
        )?;
        tx.execute("DELETE FROM samples WHERE ts < ?1", [keep_cutoff])?;
        tx.execute("DELETE FROM events WHERE ts < ?1", [keep_cutoff])?;
        tx.commit()?;
        // PASSIVE: never blocks; folds the WAL back into the main file so it stays small.
        self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `history` returns the newest N points, oldest first.
    #[test]
    fn history_keeps_newest_n_in_order() {
        let mut db = Db::open_in_memory();
        for i in 0..10 {
            db.insert_samples(1_000 + i, "cpu", &[("temp_c", i as f64)])
                .unwrap();
        }
        let pts = db.history("cpu", "temp_c", 0, 3).unwrap();
        let got: Vec<i64> = pts.iter().map(|p| p.ts).collect();
        assert_eq!(got, vec![1_007, 1_008, 1_009]);
        assert!(db.history("cpu", "load_pct", 0, 3).unwrap().is_empty());
    }

    /// Non-finite values are skipped instead of failing the whole batch.
    #[test]
    fn non_finite_values_are_dropped() {
        let mut db = Db::open_in_memory();
        db.insert_samples(5, "cpu", &[("a", f64::NAN), ("b", 1.0)])
            .unwrap();
        assert!(db.history("cpu", "a", 0, 10).unwrap().is_empty());
        assert_eq!(db.history("cpu", "b", 0, 10).unwrap().len(), 1);
    }

    /// Old minute samples collapse into hourly averages.
    #[test]
    fn maintain_downsamples_to_hourly_averages() {
        let mut db = Db::open_in_memory();
        let base = 1_699_999_200; // hour-aligned
        for i in 0..120 {
            db.insert_samples(base + i * 60, "cpu", &[("temp_c", i as f64)])
                .unwrap();
        }
        let now = base + 10 * 86_400;
        db.maintain(now, 30 * 86_400, 2 * 86_400).unwrap();
        let pts = db.history("cpu", "temp_c", 0, 100).unwrap();
        assert_eq!(pts.len(), 2);
        assert_eq!((pts[0].ts, pts[0].value), (base, 29.5));
        assert_eq!((pts[1].ts, pts[1].value), (base + 3_600, 89.5));
        // Idempotent: a second run changes nothing.
        db.maintain(now, 30 * 86_400, 2 * 86_400).unwrap();
        assert_eq!(db.history("cpu", "temp_c", 0, 100).unwrap(), pts);
    }

    /// Recent samples keep full resolution; ancient ones are deleted.
    #[test]
    fn maintain_applies_retention() {
        let mut db = Db::open_in_memory();
        let now = 2_000_000_000;
        db.insert_samples(100, "cpu", &[("temp_c", 1.0)]).unwrap();
        db.insert_samples(now - 61, "cpu", &[("temp_c", 2.0)])
            .unwrap();
        db.insert_event(100, Severity::Warn, "cpu", "old").unwrap();
        db.insert_event(now - 5, Severity::Warn, "cpu", "new")
            .unwrap();
        db.maintain(now, 30 * 86_400, 2 * 86_400).unwrap();
        let pts = db.history("cpu", "temp_c", 0, 10).unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].ts, now - 61);
        let ev = db.events(0, 10, None).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].msg, "new");
    }

    /// The severity filter applies before LIMIT.
    #[test]
    fn events_filter_by_severity() {
        let db = Db::open_in_memory();
        db.insert_event(1, Severity::Info, "a", "i").unwrap();
        db.insert_event(2, Severity::Warn, "a", "w").unwrap();
        db.insert_event(3, Severity::Crit, "a", "c").unwrap();
        db.insert_event(4, Severity::Info, "a", "i2").unwrap();
        let warn_up = db.events(0, 10, Some(Severity::Warn)).unwrap();
        let msgs: Vec<&str> = warn_up.iter().map(|e| e.msg.as_str()).collect();
        assert_eq!(msgs, vec!["w", "c"]);
        let newest = db.events(0, 1, Some(Severity::Warn)).unwrap();
        assert_eq!(newest[0].msg, "c");
    }
}
