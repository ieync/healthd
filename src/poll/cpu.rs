//! CPU poller: temperature, load and thermal-throttle events.
//!
//! Sources (all world-readable, no root needed):
//! * `/proc/stat` — cumulative jiffies, turned into a load percentage.
//! * `/sys/class/hwmon/*` — package/core temperature (coretemp, k10temp, ...).
//! * `/sys/devices/system/cpu/cpu*/thermal_throttle/*` — Intel throttle
//!   counters. AMD does not expose these, so there the metric is simply absent.
//!
//! Live values go to the hub every 5 s (RAM only). Once a minute the averages
//! are written to SQLite, which keeps disk writes to ~3 rows/min.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{file_name, list_dir, max_celsius, temp_inputs, Hub, Readings, Ticker};

/// Poll period.
///
/// WHY 5 s: fast enough to catch a thermal runaway within one fan-ramp
/// interval, slow enough that waking twice a minute-twelfth costs ~0% CPU.
const POLL_EVERY: Duration = Duration::from_secs(5);

/// Ticks per persisted sample.
///
/// WHY 12: 12 x 5 s = 1 minute, giving the spec's 1-minute average and about
/// 4 300 rows/day for the CPU.
const PERSIST_EVERY_TICKS: u32 = 12;

/// Where hwmon devices live.
const HWMON_ROOT: &str = "/sys/class/hwmon";

/// Where per-CPU sysfs directories live.
const CPU_ROOT: &str = "/sys/devices/system/cpu";

/// hwmon driver names that report CPU temperature, in preference order.
const SENSOR_NAMES: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal"];

/// Last-resort sensor: ACPI thermal zone, present on most laptops.
const FALLBACK_SENSOR: &str = "acpitz";

/// Cumulative CPU time from the first line of `/proc/stat`, in jiffies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

/// Parse the aggregate `cpu ` line of `/proc/stat`.
fn parse_stat(text: &str) -> Option<CpuTimes> {
    let mut fields = text.lines().next()?.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest is already inside user)
    let f: Vec<u64> = fields
        .take(8)
        .map(|x| x.parse().ok())
        .collect::<Option<_>>()?;
    if f.len() < 8 {
        return None;
    }
    let idle = f[3] + f[4];
    let busy = f[0] + f[1] + f[2] + f[5] + f[6] + f[7];
    Some(CpuTimes {
        busy,
        total: busy + idle,
    })
}

/// Load percentage between two snapshots; `None` if no time elapsed.
fn load_pct(prev: CpuTimes, cur: CpuTimes) -> Option<f64> {
    let total = cur.total.checked_sub(prev.total).filter(|&t| t > 0)?;
    let busy = cur.busy.saturating_sub(prev.busy);
    Some(busy as f64 / total as f64 * 100.0)
}

/// Find the temperature inputs of the CPU sensor under `hwmon_root`.
///
/// Prefers real CPU drivers; only falls back to `acpitz` if none exist.
fn find_temp_inputs(hwmon_root: &Path) -> Vec<PathBuf> {
    let mut preferred = Vec::new();
    let mut fallback = Vec::new();
    for dir in list_dir(hwmon_root) {
        let name = fs::read_to_string(dir.join("name")).unwrap_or_default();
        let name = name.trim();
        let bucket = if SENSOR_NAMES.contains(&name) {
            &mut preferred
        } else if name == FALLBACK_SENSOR {
            &mut fallback
        } else {
            continue;
        };
        bucket.extend(temp_inputs(&dir));
    }
    if preferred.is_empty() {
        fallback
    } else {
        preferred
    }
}

/// Find Intel `core_throttle_count` / `package_throttle_count` files.
fn find_throttle_counters(cpu_root: &Path) -> Vec<PathBuf> {
    list_dir(cpu_root)
        .into_iter()
        .filter(|d| {
            let n = file_name(d);
            n.starts_with("cpu") && n[3..].chars().all(|c| c.is_ascii_digit()) && n.len() > 3
        })
        .flat_map(|d| {
            let dir = d.join("thermal_throttle");
            [
                dir.join("core_throttle_count"),
                dir.join("package_throttle_count"),
            ]
        })
        .filter(|p| p.is_file())
        .collect()
}

/// Sum of all throttle counters (unreadable ones count as 0).
fn read_throttle_total(files: &[PathBuf]) -> u64 {
    files
        .iter()
        .filter_map(|f| fs::read_to_string(f).ok()?.trim().parse::<u64>().ok())
        .sum()
}

/// Running totals for the current persistence window.
#[derive(Default)]
struct Accum {
    temp_sum: f64,
    temp_n: u32,
    load_sum: f64,
    load_n: u32,
    throttle: u64,
    ticks: u32,
}

impl Accum {
    /// Add one 5-second observation.
    fn add(&mut self, temp: Option<f64>, load: Option<f64>, throttle: u64) {
        if let Some(t) = temp {
            self.temp_sum += t;
            self.temp_n += 1;
        }
        if let Some(l) = load {
            self.load_sum += l;
            self.load_n += 1;
        }
        self.throttle += throttle;
        self.ticks += 1;
    }

    /// Readings to persist: averages for temp/load, a *sum* for throttle events.
    ///
    /// WHY sum for throttle: it is a count of rare events; averaging 3 events
    /// over a minute down to 0.25 would hide exactly what we want to see.
    fn to_store(&self, has_throttle: bool) -> Readings {
        let mut out = Readings::new();
        if self.temp_n > 0 {
            out.push(("temp_c", self.temp_sum / f64::from(self.temp_n)));
        }
        if self.load_n > 0 {
            out.push(("load_pct", self.load_sum / f64::from(self.load_n)));
        }
        if has_throttle {
            out.push(("throttle_events", self.throttle as f64));
        }
        out
    }
}

/// Everything the poll loop remembers between ticks.
#[derive(Default)]
struct State {
    temp_files: Vec<PathBuf>,
    throttle_files: Vec<PathBuf>,
    prev_times: Option<CpuTimes>,
    prev_throttle: Option<u64>,
    acc: Accum,
}

impl State {
    /// (Re)discover sensor files; hwmon numbers can change across suspend or
    /// module reloads, so this runs once per persisted minute.
    fn discover(&mut self) {
        self.temp_files = find_temp_inputs(Path::new(HWMON_ROOT));
        self.throttle_files = find_throttle_counters(Path::new(CPU_ROOT));
    }
}

/// One 5-second poll.
fn tick(hub: &Hub, st: &mut State) {
    let temp = max_celsius(&st.temp_files);

    let mut load = None;
    if let Some(cur) = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|t| parse_stat(&t))
    {
        load = st.prev_times.and_then(|prev| load_pct(prev, cur));
        st.prev_times = Some(cur);
    }

    let has_throttle = !st.throttle_files.is_empty();
    let mut throttle_delta = 0;
    if has_throttle {
        let total = read_throttle_total(&st.throttle_files);
        throttle_delta = st.prev_throttle.map_or(0, |p| total.saturating_sub(p));
        st.prev_throttle = Some(total);
    }

    let mut now = Readings::new();
    if let Some(t) = temp {
        now.push(("temp_c", t));
    }
    if let Some(l) = load {
        now.push(("load_pct", l));
    }
    if has_throttle {
        now.push(("throttle_events", throttle_delta as f64));
    }
    hub.observe("cpu", &now);

    st.acc.add(temp, load, throttle_delta);
    if st.acc.ticks >= PERSIST_EVERY_TICKS {
        hub.store("cpu", &st.acc.to_store(has_throttle));
        st.acc = Accum::default();
        st.discover();
    }
}

/// Poll loop; never returns (the supervisor restarts it if it ever does).
pub fn run(hub: &Hub) {
    let mut st = State::default();
    st.discover();
    let mut ticker = Ticker::new(POLL_EVERY);
    loop {
        tick(hub, &mut st);
        ticker.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory for fake sysfs trees.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("healthd-cpu-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `/proc/stat` parsing and load maths.
    #[test]
    fn stat_and_load() {
        let a = parse_stat("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n").unwrap();
        assert_eq!(
            a,
            CpuTimes {
                busy: 150,
                total: 1_000
            }
        );
        let b = parse_stat("cpu  200 0 100 850 50 0 0 0 0 0\n").unwrap();
        // Delta: busy +150, total +200 -> 75 %
        assert_eq!(load_pct(a, b), Some(75.0));
        assert_eq!(load_pct(b, b), None);
        assert!(parse_stat("cpu0 1 2 3").is_none());
        assert!(parse_stat("garbage").is_none());
    }

    /// The preferred sensor wins; acpitz is only a fallback.
    #[test]
    fn sensor_discovery() {
        let root = scratch("hwmon");
        for (dir, name, files) in [
            ("hwmon0", "acpitz", vec!["temp1_input"]),
            ("hwmon1", "nvme", vec!["temp1_input"]),
            (
                "hwmon2",
                "coretemp",
                vec!["temp1_input", "temp2_input", "temp2_label"],
            ),
        ] {
            let d = root.join(dir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("name"), format!("{name}\n")).unwrap();
            for f in files {
                fs::write(d.join(f), "45000\n").unwrap();
            }
        }
        let found = find_temp_inputs(&root);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|p| p.to_string_lossy().contains("hwmon2")));
        fs::write(root.join("hwmon2/temp1_input"), "61500\n").unwrap();
        assert_eq!(max_celsius(&found), Some(61.5));

        fs::remove_dir_all(root.join("hwmon2")).unwrap();
        let fallback = find_temp_inputs(&root);
        assert_eq!(fallback.len(), 1);
        assert!(fallback[0].to_string_lossy().contains("hwmon0"));
        let _ = fs::remove_dir_all(&root);
    }

    /// Throttle counters are found per CPU and summed.
    #[test]
    fn throttle_counters() {
        let root = scratch("cpu");
        for cpu in ["cpu0", "cpu1", "cpufreq", "cpuidle"] {
            let d = root.join(cpu).join("thermal_throttle");
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("core_throttle_count"), "3\n").unwrap();
            fs::write(d.join("package_throttle_count"), "4\n").unwrap();
        }
        let files = find_throttle_counters(&root);
        assert_eq!(files.len(), 4); // cpufreq / cpuidle are ignored
        assert_eq!(read_throttle_total(&files), 14);
        let _ = fs::remove_dir_all(&root);
    }

    /// Persisted values are averages, except throttle which is a sum.
    #[test]
    fn accumulator_averages() {
        let mut acc = Accum::default();
        acc.add(Some(50.0), Some(10.0), 0);
        acc.add(Some(60.0), Some(30.0), 2);
        acc.add(None, None, 1);
        let out = acc.to_store(true);
        assert_eq!(
            out,
            vec![
                ("temp_c", 55.0),
                ("load_pct", 20.0),
                ("throttle_events", 3.0)
            ]
        );
        assert_eq!(acc.to_store(false).len(), 2);
    }
}
