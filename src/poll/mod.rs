//! Poller framework: the shared [`Hub`], supervised poller threads, drift-free
//! sleeping, and the small sysfs helpers every poller needs.
//!
//! Each data source lives in its own submodule with a `run(&Hub)` loop. All of
//! them push into the [`Hub`], which keeps the live values, evaluates alert
//! rules and writes to SQLite; the IPC server reads from the same hub.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{fs, io};

use healthd_proto::{now_ts, EventsData, HistoryData, Metric, Severity, StatusData};

use crate::alert::{Engine, Notifier, Transition};
use crate::config::{Config, Retention};
use crate::db::Db;

pub mod battery;
pub mod cpu;
pub mod nvme;
pub mod ram;

/// Metric readings produced by one poll: `(metric name, value)`.
pub type Readings = Vec<(&'static str, f64)>;

/// Stack size for every daemon thread.
///
/// WHY 256 KiB: pollers parse a few KB of text and never recurse deeply; the
/// 2 MiB default is address space we never touch, so this mostly keeps the
/// process's virtual size honest.
const STACK_BYTES: usize = 256 * 1024;

/// Delay before restarting a poller that crashed.
///
/// WHY 30 s: long enough that a poller failing instantly cannot spin a CPU
/// core, short enough that a one-off glitch costs well under a minute of data.
const RESTART_DELAY: Duration = Duration::from_secs(30);

/// Default look-back for `history` and `events` requests: one day.
const DEFAULT_LOOKBACK_SECS: i64 = 86_400;

/// Key of the live-value map: (source, metric).
type MetricKey = (String, String);

/// Lock a mutex, ignoring poisoning.
///
/// WHY: a panicking poller must not take every other thread down with it; the
/// guarded data (a map, a DB handle) stays structurally valid.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Live values and alert state, guarded together.
struct State {
    /// Latest (timestamp, value) per metric — all RAM, no DB involved.
    latest: BTreeMap<MetricKey, (i64, f64)>,
    /// Rule engine.
    engine: Engine,
}

/// Everything pollers write to and the IPC server reads from.
pub struct Hub {
    db: Mutex<Db>,
    state: Mutex<State>,
    notifier: Notifier,
    started: Instant,
    retention: Retention,
    battery_name: String,
    nvme_command: String,
}

impl Hub {
    /// Build the hub around an open database and the loaded config.
    pub fn new(db: Db, cfg: &Config) -> Arc<Self> {
        Arc::new(Self {
            db: Mutex::new(db),
            state: Mutex::new(State {
                latest: BTreeMap::new(),
                engine: Engine::new(cfg.rules.clone()),
            }),
            notifier: Notifier::start(cfg.notify.desktop),
            started: Instant::now(),
            retention: cfg.retention,
            battery_name: cfg.battery.name.clone(),
            nvme_command: cfg.nvme.command.clone(),
        })
    }

    /// Command name typed after `doas -n` when reading NVMe SMART data.
    pub fn nvme_command(&self) -> &str {
        &self.nvme_command
    }

    /// Name of the battery to poll under `/sys/class/power_supply/`.
    pub fn battery_name(&self) -> &str {
        &self.battery_name
    }

    /// Record live values and evaluate alert rules; touches no disk unless an
    /// alert fires.
    pub fn observe(&self, source: &str, readings: &[(&'static str, f64)]) {
        let ts = now_ts();
        let mut transitions = Vec::new();
        {
            let mut st = lock(&self.state);
            for &(metric, value) in readings {
                st.latest
                    .insert((source.to_owned(), metric.to_owned()), (ts, value));
                transitions.extend(st.engine.observe(ts, source, metric, value));
            }
        }
        // Locks are released before any I/O.
        self.dispatch(ts, transitions);
    }

    /// Persist readings as samples in one transaction.
    pub fn store(&self, source: &str, readings: &[(&'static str, f64)]) {
        if readings.is_empty() {
            return;
        }
        let result = lock(&self.db).insert_samples(now_ts(), source, readings);
        if let Err(e) = result {
            eprintln!("healthd: storing {source} samples failed: {e}");
        }
    }

    /// Convenience: [`observe`](Self::observe) and, if `persist`, [`store`](Self::store).
    pub fn record(&self, source: &str, readings: &[(&'static str, f64)], persist: bool) {
        self.observe(source, readings);
        if persist {
            self.store(source, readings);
        }
    }

    /// Write a diagnostic event without a desktop notification.
    pub fn log_event(&self, severity: Severity, source: &str, msg: &str) {
        let result = lock(&self.db).insert_event(now_ts(), severity, source, msg);
        if let Err(e) = result {
            eprintln!("healthd: storing event failed: {e}");
        }
    }

    /// Persist engine transitions as events and notify where requested.
    fn dispatch(&self, ts: i64, transitions: Vec<Transition>) {
        for t in transitions {
            let result = lock(&self.db).insert_event(ts, t.severity, &t.source, &t.msg);
            if let Err(e) = result {
                eprintln!("healthd: storing event failed: {e}");
            }
            if t.notify {
                self.notifier.send(t.severity, &t.source, &t.msg);
            }
        }
    }

    /// Snapshot for the `status` command (served entirely from RAM).
    pub fn status(&self) -> StatusData {
        let st = lock(&self.state);
        StatusData {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            uptime_secs: self.started.elapsed().as_secs(),
            metrics: st
                .latest
                .iter()
                .map(|((source, metric), &(ts, value))| Metric {
                    source: source.clone(),
                    metric: metric.clone(),
                    value,
                    ts,
                })
                .collect(),
            active_alerts: st.engine.active(),
        }
    }

    /// Stored samples for the `history` command.
    pub fn history(
        &self,
        source: &str,
        metric: &str,
        since: Option<i64>,
        limit: Option<u32>,
    ) -> rusqlite::Result<HistoryData> {
        let since = since.unwrap_or_else(|| now_ts() - DEFAULT_LOOKBACK_SECS);
        // WHY 1 000 / 10 000: 1 000 points is more than any terminal chart can
        // draw columns for; 10 000 is ~7 days of 1-minute data and bounds reply
        // size and query time.
        let limit = limit.unwrap_or(1_000).clamp(1, 10_000);
        let points = lock(&self.db).history(source, metric, since, limit)?;
        Ok(HistoryData {
            source: source.to_owned(),
            metric: metric.to_owned(),
            points,
        })
    }

    /// Stored events for the `events` command.
    pub fn events(
        &self,
        since: Option<i64>,
        limit: Option<u32>,
        min_severity: Option<Severity>,
    ) -> rusqlite::Result<EventsData> {
        let since = since.unwrap_or_else(|| now_ts() - DEFAULT_LOOKBACK_SECS);
        // WHY 100 / 1 000: a day of alerts is normally a handful of lines, so 100
        // is plenty by default; 1 000 events (~100 KB of JSON) is the most one
        // reply should ever carry.
        let limit = limit.unwrap_or(100).clamp(1, 1_000);
        let events = lock(&self.db).events(since, limit, min_severity)?;
        Ok(EventsData { events })
    }

    /// Run retention and downsampling now.
    pub fn maintain(&self) {
        let keep = i64::from(self.retention.days) * 86_400;
        let raw = i64::from(self.retention.raw_hours) * 3_600;
        let result = lock(&self.db).maintain(now_ts(), keep, raw);
        if let Err(e) = result {
            eprintln!("healthd: maintenance failed: {e}");
        }
    }
}

/// Spawn `body` as a supervised thread.
///
/// If `body` panics (or returns, which it should never do) the supervisor logs
/// a critical event and restarts it after [`RESTART_DELAY`], so one buggy
/// parser cannot silently end monitoring for the rest of the daemon's life.
pub fn spawn(name: &'static str, hub: &Arc<Hub>, body: fn(&Hub)) -> io::Result<JoinHandle<()>> {
    let hub = Arc::clone(hub);
    thread::Builder::new()
        .name(name.to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || loop {
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| body(&hub)));
            let how = if outcome.is_err() {
                "panicked"
            } else {
                "exited"
            };
            let msg = format!(
                "{name} poller {how}; restarting in {}s",
                RESTART_DELAY.as_secs()
            );
            eprintln!("healthd: {msg}");
            hub.log_event(Severity::Crit, "healthd", &msg);
            thread::sleep(RESTART_DELAY);
        })
}

/// Fixed-rate sleeper.
///
/// Sleeps until `start + n * every` instead of "`every` after the work
/// finished", so slow polls do not accumulate drift. After a long stall
/// (laptop suspend) it resynchronises instead of firing a burst of catch-up
/// ticks.
pub struct Ticker {
    every: Duration,
    next: Instant,
}

impl Ticker {
    /// Create a ticker; the first deadline is now, so the first poll is immediate.
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            next: Instant::now(),
        }
    }

    /// Block until the next tick.
    pub fn wait(&mut self) {
        self.next += self.every;
        let now = Instant::now();
        match self.next.checked_duration_since(now) {
            Some(remaining) => thread::sleep(remaining),
            None => self.next = now,
        }
    }
}

/// Sorted entries of a directory; empty if it cannot be read.
pub fn list_dir(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    paths.sort();
    paths
}

/// File name of `path` as `&str` (empty if not UTF-8).
pub fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("")
}

/// All `temp*_input` files directly inside a hwmon directory.
pub fn temp_inputs(hwmon_dir: &Path) -> Vec<PathBuf> {
    list_dir(hwmon_dir)
        .into_iter()
        .filter(|p| {
            let n = file_name(p);
            n.starts_with("temp") && n.ends_with("_input")
        })
        .collect()
}

/// Read a hwmon temperature file (millidegrees Celsius) as degrees Celsius.
pub fn read_celsius(path: &Path) -> Option<f64> {
    let milli: f64 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
    Some(milli / 1000.0)
}

/// Hottest reading across `files`, if any could be read.
pub fn max_celsius(files: &[PathBuf]) -> Option<f64> {
    files
        .iter()
        .filter_map(|f| read_celsius(f))
        .reduce(f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Hub wired to an in-memory DB with notifications off.
    fn test_hub() -> Arc<Hub> {
        let mut cfg = Config::builtin().unwrap();
        cfg.notify.desktop = false;
        Hub::new(Db::open_in_memory(), &cfg)
    }

    /// `observe` feeds `status` from RAM only; `store` feeds `history`.
    #[test]
    fn observe_is_ram_only_and_store_persists() {
        let hub = test_hub();
        hub.observe("cpu", &[("temp_c", 55.0)]);
        let status = hub.status();
        assert_eq!(status.metrics.len(), 1);
        assert_eq!(status.metrics[0].value, 55.0);
        assert!(hub
            .history("cpu", "temp_c", Some(0), None)
            .unwrap()
            .points
            .is_empty());
        hub.store("cpu", &[("temp_c", 55.0)]);
        assert_eq!(
            hub.history("cpu", "temp_c", Some(0), None)
                .unwrap()
                .points
                .len(),
            1
        );
    }

    /// A firing rule shows up as an event and as an active alert.
    #[test]
    fn alerts_become_events() {
        let hub = test_hub();
        hub.observe("nvme0", &[("media_errors", 2.0)]);
        let events = hub.events(Some(0), None, None).unwrap().events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, Severity::Crit);
        assert_eq!(hub.status().active_alerts.len(), 1);
    }

    /// The ticker keeps its schedule instead of drifting.
    #[test]
    fn ticker_waits_roughly_one_period() {
        let mut t = Ticker::new(Duration::from_millis(30));
        let start = Instant::now();
        t.wait();
        t.wait();
        assert!(start.elapsed() >= Duration::from_millis(55));
    }
}
