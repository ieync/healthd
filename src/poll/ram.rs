//! RAM poller: available memory and out-of-memory risk.
//!
//! Reads `/proc/meminfo` (and `/proc/pressure/memory` where the kernel has
//! PSI) every 5 s. Nothing is stored as samples: RAM readings change too fast
//! to be worth the disk writes, and only *events* (rule violations) matter
//! later. Live values are still available through `status`.

use std::fs;
use std::time::Duration;

use super::{Hub, Readings, Ticker};

/// Poll period.
///
/// WHY 5 s: a runaway allocation can eat gigabytes in seconds, so 60 s would
/// miss it. 5 s costs two tiny procfs reads and keeps the alert early enough
/// to act on before the OOM killer.
const POLL_EVERY: Duration = Duration::from_secs(5);

/// Memory counters we need from `/proc/meminfo`, in kB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MemInfo {
    available_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
}

/// Parse the `Key:   value kB` lines of `/proc/meminfo`.
fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let (mut available, mut swap_total, mut swap_free) = (None, None, None);
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "MemAvailable" => &mut available,
            "SwapTotal" => &mut swap_total,
            "SwapFree" => &mut swap_free,
            _ => continue,
        };
        *slot = rest.split_whitespace().next().and_then(|n| n.parse().ok());
    }
    Some(MemInfo {
        available_kb: available?,
        swap_total_kb: swap_total?,
        swap_free_kb: swap_free?,
    })
}

/// Extract `avg10` of the `some` line from `/proc/pressure/memory`.
///
/// `some avg10` is the share of the last 10 s in which at least one task was
/// stalled waiting for memory — a direct measure of thrashing.
fn parse_psi_some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|tok| tok.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// Turn raw counters into metrics.
///
/// `oom_headroom_mb` is MemAvailable plus free swap: everything the kernel can
/// still hand out before it has to kill something.
fn readings(mem: &MemInfo, psi: Option<f64>) -> Readings {
    let mb = |kb: u64| kb as f64 / 1024.0;
    let mut out = vec![
        ("available_mb", mb(mem.available_kb)),
        ("oom_headroom_mb", mb(mem.available_kb + mem.swap_free_kb)),
    ];
    if mem.swap_total_kb > 0 {
        let used = mem.swap_total_kb.saturating_sub(mem.swap_free_kb);
        out.push((
            "swap_used_pct",
            used as f64 / mem.swap_total_kb as f64 * 100.0,
        ));
    }
    if let Some(p) = psi {
        out.push(("psi_some_avg10", p));
    }
    out
}

/// Poll loop; never returns (the supervisor restarts it if it ever does).
pub fn run(hub: &Hub) {
    let mut ticker = Ticker::new(POLL_EVERY);
    loop {
        if let Some(mem) = fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|t| parse_meminfo(&t))
        {
            let psi = fs::read_to_string("/proc/pressure/memory")
                .ok()
                .and_then(|t| parse_psi_some_avg10(&t));
            hub.observe("ram", &readings(&mem, psi));
        }
        ticker.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real-world `/proc/meminfo`.
    const MEMINFO: &str = "MemTotal:       16318464 kB\nMemFree:         2048000 kB\n\
        MemAvailable:    8192000 kB\nBuffers:          100000 kB\n\
        SwapTotal:       4194304 kB\nSwapFree:        3145728 kB\n";

    /// meminfo parsing and derived metrics.
    #[test]
    fn meminfo_and_readings() {
        let mem = parse_meminfo(MEMINFO).unwrap();
        assert_eq!(mem.available_kb, 8_192_000);
        let r = readings(&mem, Some(1.5));
        assert_eq!(r[0], ("available_mb", 8_000.0));
        assert_eq!(r[1], ("oom_headroom_mb", 8_000.0 + 3_072.0));
        assert_eq!(r[2], ("swap_used_pct", 25.0));
        assert_eq!(r[3], ("psi_some_avg10", 1.5));
        assert!(parse_meminfo("MemTotal: 1 kB\n").is_none());
    }

    /// No swap means no swap metric (and no division by zero).
    #[test]
    fn no_swap() {
        let mem = MemInfo {
            available_kb: 1024,
            swap_total_kb: 0,
            swap_free_kb: 0,
        };
        let r = readings(&mem, None);
        assert_eq!(r.len(), 2);
    }

    /// PSI line parsing.
    #[test]
    fn psi() {
        let text = "some avg10=12.34 avg60=5.00 avg300=1.00 total=123456\n\
                    full avg10=2.00 avg60=1.00 avg300=0.50 total=1234\n";
        assert_eq!(parse_psi_some_avg10(text), Some(12.34));
        assert_eq!(parse_psi_some_avg10("nonsense"), None);
    }
}
