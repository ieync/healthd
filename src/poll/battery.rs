//! Battery poller: charge, health vs design capacity, cycles and "abuse".
//!
//! Reads `/sys/class/power_supply/<name>/uevent` once a minute — one file read
//! gives every field, and it needs no root.
//!
//! "Abuse" signals derived from it:
//! * `hours_held_full` — how long the battery has sat at 95%+ while not
//!   discharging (high state of charge is the classic Li-ion wear factor).
//! * `discharging_pct` — charge level, emitted *only* while discharging, so a
//!   "deep discharge" rule cannot fire while plugged in.
//! * `temp_c` — heat, where the battery reports it.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use healthd_proto::now_ts;

use super::{Hub, Readings, Ticker};

/// Poll period.
///
/// WHY 60 s: battery state changes over minutes to hours, and the uevent file
/// is read by firmware via ACPI/EC, which is slow enough that polling it every
/// few seconds would itself cost power.
const POLL_EVERY: Duration = Duration::from_secs(60);

/// Charge level at which the battery counts as "held full".
///
/// WHY 95 %: chargers taper and many firmwares stop at 95-99 %, so a strict
/// 100 % test would miss batteries that are in fact parked near the top.
const HELD_FULL_PCT: f64 = 95.0;

/// Where power supplies live.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// The fields of `uevent` we care about.
#[derive(Debug, Default, PartialEq)]
struct Uevent {
    status: Option<String>,
    capacity: Option<f64>,
    cycles: Option<f64>,
    full: Option<f64>,
    design: Option<f64>,
    temp_tenths: Option<f64>,
    present: Option<bool>,
}

/// Parse `POWER_SUPPLY_*=value` lines.
///
/// Batteries report either energy (µWh) or charge (µAh); the *ratio* of full to
/// design is the same either way, so both are accepted.
fn parse_uevent(text: &str) -> Uevent {
    let mut u = Uevent::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let num = || value.trim().parse::<f64>().ok();
        match key {
            "POWER_SUPPLY_STATUS" => u.status = Some(value.trim().to_owned()),
            "POWER_SUPPLY_CAPACITY" => u.capacity = num(),
            "POWER_SUPPLY_CYCLE_COUNT" => u.cycles = num().filter(|c| *c >= 0.0),
            "POWER_SUPPLY_ENERGY_FULL" | "POWER_SUPPLY_CHARGE_FULL" => u.full = num(),
            "POWER_SUPPLY_ENERGY_FULL_DESIGN" | "POWER_SUPPLY_CHARGE_FULL_DESIGN" => {
                u.design = num();
            }
            "POWER_SUPPLY_TEMP" => u.temp_tenths = num(),
            "POWER_SUPPLY_PRESENT" => u.present = Some(value.trim() == "1"),
            _ => {}
        }
    }
    u
}

/// Metrics derived from one poll, split by what to do with them.
#[derive(Debug, Default, PartialEq)]
struct Derived {
    /// Persisted every minute.
    stored: Readings,
    /// Live-only (alert rules and `status`), not worth disk space.
    transient: Readings,
}

/// Compute metrics; `held_since` is the Unix time a "held full" stretch began.
///
/// WHY wall-clock seconds and not `Instant`: `Instant` stops counting while the
/// machine is suspended, but a laptop asleep on its charger is still sitting at
/// 100 % and that time should count. (The stretch is forgotten on daemon
/// restart; that only ever under-reports.)
fn derive(u: &Uevent, held_since: &mut Option<i64>, now: i64) -> Derived {
    let mut d = Derived::default();
    let discharging = u.status.as_deref() == Some("Discharging");

    if let Some(c) = u.capacity {
        d.stored.push(("charge_pct", c));
        if discharging {
            d.transient.push(("discharging_pct", c));
        }
        if c >= HELD_FULL_PCT && !discharging {
            held_since.get_or_insert(now);
        } else {
            *held_since = None;
        }
        let hours = held_since.map_or(0.0, |t| (now - t).max(0) as f64 / 3_600.0);
        d.stored.push(("hours_held_full", hours));
    }
    if let (Some(full), Some(design)) = (u.full, u.design) {
        if design > 0.0 {
            d.stored.push(("health_pct", full / design * 100.0));
        }
    }
    if let Some(cycles) = u.cycles {
        d.stored.push(("cycles", cycles));
    }
    if let Some(t) = u.temp_tenths {
        d.stored.push(("temp_c", t / 10.0));
    }
    d
}

/// Poll loop; never returns (the supervisor restarts it if it ever does).
pub fn run(hub: &Hub) {
    let path: PathBuf = [POWER_SUPPLY_ROOT, hub.battery_name(), "uevent"]
        .iter()
        .collect();
    let mut held_since = None;
    let mut ticker = Ticker::new(POLL_EVERY);
    loop {
        // A missing file just means "no battery" (desktop PC): skip quietly and
        // look again next minute in case one is hot-plugged.
        if let Ok(text) = fs::read_to_string(&path) {
            let u = parse_uevent(&text);
            if u.present != Some(false) {
                let d = derive(&u, &mut held_since, now_ts());
                let mut all = d.stored.clone();
                all.extend(d.transient);
                hub.observe("battery", &all);
                hub.store("battery", &d.stored);
            }
        }
        ticker.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A discharging ThinkPad battery at 80 % health.
    const SAMPLE: &str = "POWER_SUPPLY_NAME=BAT0\nPOWER_SUPPLY_TYPE=Battery\n\
        POWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_PRESENT=1\n\
        POWER_SUPPLY_CYCLE_COUNT=212\nPOWER_SUPPLY_ENERGY_FULL_DESIGN=57000000\n\
        POWER_SUPPLY_ENERGY_FULL=45600000\nPOWER_SUPPLY_ENERGY_NOW=22800000\n\
        POWER_SUPPLY_CAPACITY=8\nPOWER_SUPPLY_TEMP=312\n";

    /// Look a metric up by name.
    fn get(r: &Readings, name: &str) -> Option<f64> {
        r.iter().find(|(m, _)| *m == name).map(|&(_, v)| v)
    }

    /// uevent parsing and the derived metrics.
    #[test]
    fn parses_and_derives() {
        let u = parse_uevent(SAMPLE);
        assert_eq!(u.status.as_deref(), Some("Discharging"));
        assert_eq!(u.present, Some(true));
        let mut held = None;
        let d = derive(&u, &mut held, 1_000);
        assert!((get(&d.stored, "health_pct").unwrap() - 80.0).abs() < 1e-9);
        assert_eq!(get(&d.stored, "cycles"), Some(212.0));
        assert_eq!(get(&d.stored, "charge_pct"), Some(8.0));
        assert_eq!(get(&d.stored, "temp_c"), Some(31.2));
        assert_eq!(get(&d.transient, "discharging_pct"), Some(8.0));
        assert_eq!(get(&d.stored, "hours_held_full"), Some(0.0));
        assert!(held.is_none());
    }

    /// Charge-based (µAh) batteries work too; missing fields are skipped.
    #[test]
    fn charge_based_and_sparse() {
        let u = parse_uevent(
            "POWER_SUPPLY_CHARGE_FULL=4000000\nPOWER_SUPPLY_CHARGE_FULL_DESIGN=5000000\n\
             POWER_SUPPLY_CYCLE_COUNT=-1\n",
        );
        let d = derive(&u, &mut None, 1_000);
        assert_eq!(d.stored, vec![("health_pct", 80.0)]);
    }

    /// Time spent at full charge on AC accumulates; discharging resets it.
    #[test]
    fn held_full_tracking() {
        let full = parse_uevent("POWER_SUPPLY_STATUS=Full\nPOWER_SUPPLY_CAPACITY=100\n");
        let start = 1_000;
        let mut held = None;
        derive(&full, &mut held, start);
        assert_eq!(held, Some(start));
        let later = start + 36 * 3_600;
        let d = derive(&full, &mut held, later);
        assert_eq!(get(&d.stored, "hours_held_full"), Some(36.0));
        assert!(get(&d.transient, "discharging_pct").is_none());

        let unplugged = parse_uevent("POWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_CAPACITY=99\n");
        let d = derive(&unplugged, &mut held, later);
        assert_eq!(get(&d.stored, "hours_held_full"), Some(0.0));
        assert!(held.is_none());
    }
}
