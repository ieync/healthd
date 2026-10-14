//! NVMe poller: temperature from sysfs, wear and error counters from SMART.
//!
//! * Temperature: `/sys/block/nvme*/device/hwmon*/temp*_input` — world
//!   readable, polled every 60 s.
//! * SMART health log: only the drive can tell us `percentage_used`, spare,
//!   media errors, critical warnings and unsafe shutdowns, and reading it
//!   needs root. We run `doas -n nvme smart-log` (non-interactive: it fails
//!   instead of prompting when no `permit nopass` rule exists) every 10 min.
//!
//! Each controller is its own source (`nvme0`, `nvme1`, ...).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use healthd_proto::Severity;
use serde_json::Value;

use super::{file_name, list_dir, max_celsius, temp_inputs, Hub, Readings, Ticker};

/// Temperature poll period.
///
/// WHY 60 s: NVMe heatsinks and drive mass smooth temperature over tens of
/// seconds, so a minute catches sustained heat while keeping wake-ups rare.
const POLL_EVERY: Duration = Duration::from_secs(60);

/// Ticks between SMART reads.
///
/// WHY 10 (= 10 min): wear counters change over days, and each read spawns
/// `doas` + `nvme` and sends an admin command to the controller. Ten minutes
/// still notices new media errors quickly.
const SMART_EVERY_TICKS: u32 = 10;

/// Kill `doas nvme` if it has not finished by then.
///
/// WHY 15 s: a healthy call takes milliseconds; anything longer means a hung
/// drive or a stuck prompt, and a stuck child must not block monitoring.
const SMART_TIMEOUT: Duration = Duration::from_secs(15);

/// How often to check whether the `doas nvme` child has finished.
///
/// WHY 100 ms: std has no timed `wait`, so we poll. The call normally ends in a
/// few milliseconds, so this adds a negligible delay, while a hung call costs
/// only ~150 wake-ups before `SMART_TIMEOUT` kills it (once per 10 minutes).
const CHILD_POLL: Duration = Duration::from_millis(100);

/// Block devices live here.
const SYS_BLOCK: &str = "/sys/block";

/// `nvme0n1` -> `nvme0` (the controller name, which is what `/dev/nvmeX` is).
fn controller_of(block: &str) -> &str {
    match block.rfind('n') {
        Some(i) if i > 4 && block.starts_with("nvme") => &block[..i],
        _ => block,
    }
}

/// Find NVMe controllers and their hwmon temperature files.
///
/// Several namespaces of one controller share one hwmon device, so results are
/// keyed by controller to avoid double counting.
fn discover(sys_block: &Path) -> BTreeMap<String, Vec<PathBuf>> {
    let mut drives: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for block in list_dir(sys_block) {
        let name = file_name(&block);
        if !name.starts_with("nvme") {
            continue;
        }
        let device = block.join("device");
        let files: Vec<PathBuf> = list_dir(&device)
            .into_iter()
            .filter(|d| file_name(d).starts_with("hwmon"))
            .flat_map(|d| temp_inputs(&d))
            .collect();
        drives
            .entry(controller_of(name).to_owned())
            .or_insert(files);
    }
    drives
}

/// Read a number from the first matching key of a JSON object.
fn json_num(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| v.get(k)?.as_f64())
}

/// Parse `nvme smart-log -o json` into readings.
///
/// Key names differ slightly between nvme-cli versions (`percent_used` vs
/// `percentage_used`), so alternatives are accepted; the stored metric is
/// always `percentage_used`.
fn parse_smart_log(json: &str) -> Option<Readings> {
    let v: Value = serde_json::from_str(json).ok()?;
    let fields: [(&'static str, &[&str]); 5] = [
        ("percentage_used", &["percent_used", "percentage_used"]),
        ("avail_spare", &["avail_spare", "available_spare"]),
        ("media_errors", &["media_errors"]),
        ("critical_warning", &["critical_warning"]),
        ("unsafe_shutdowns", &["unsafe_shutdowns"]),
    ];
    let mut out: Readings = fields
        .iter()
        .filter_map(|&(metric, keys)| Some((metric, json_num(&v, keys)?)))
        .collect();
    if let Some(t) = json_num(&v, &["temperature"]) {
        // nvme-cli reports Kelvin; no drive survives 200 C, so a larger value
        // is Kelvin and a smaller one is already Celsius.
        let celsius = if t > 200.0 { t - 273.15 } else { t };
        out.push(("temp_c", celsius));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Arguments handed to `doas` to read one controller's SMART log.
///
/// Kept in one place so the command that runs, the command the failure event
/// prints and the README's doas rule cannot drift apart. `nvme` is the command
/// *as typed*: doas matches rules against that string, which is why it is
/// configurable (`[nvme] command`).
fn smart_args(nvme: &str, ctrl: &str) -> Vec<String> {
    [
        "-n",
        nvme,
        "smart-log",
        &format!("/dev/{ctrl}"),
        "-o",
        "json",
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Run `doas -n <nvme> smart-log /dev/<ctrl> -o json` with the standard timeout.
fn run_smart_log(nvme: &str, ctrl: &str) -> Result<String, String> {
    run_smart_log_with("doas", nvme, ctrl, SMART_TIMEOUT)
}

/// [`run_smart_log`] with the privilege helper and timeout as parameters, so
/// tests can substitute a fake `doas` script.
fn run_smart_log_with(
    program: &str,
    nvme: &str,
    ctrl: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(smart_args(nvme, ctrl))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;

    // The output is ~1 KB, far below the pipe buffer, so the child can exit
    // without us reading; we only read after it is done.
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("doas/nvme failed: {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(CHILD_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("timed out".to_owned());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .ok_or("no stdout")?
        .read_to_string(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// Poll loop; never returns (the supervisor restarts it if it ever does).
pub fn run(hub: &Hub) {
    let mut ticker = Ticker::new(POLL_EVERY);
    let mut smart_countdown = 0_u32;
    let mut smart_failure_reported = false;
    loop {
        let do_smart = smart_countdown == 0;
        smart_countdown = if do_smart {
            SMART_EVERY_TICKS - 1
        } else {
            smart_countdown - 1
        };

        for (ctrl, files) in &discover(Path::new(SYS_BLOCK)) {
            let mut readings = Readings::new();
            let sysfs_temp = max_celsius(files);
            if let Some(t) = sysfs_temp {
                readings.push(("temp_c", t));
            }
            if do_smart {
                match run_smart_log(hub.nvme_command(), ctrl).and_then(|j| {
                    parse_smart_log(&j).ok_or_else(|| "unrecognised smart-log output".to_owned())
                }) {
                    Ok(mut smart) => {
                        smart_failure_reported = false;
                        if sysfs_temp.is_some() {
                            // Prefer the fresher sysfs temperature.
                            smart.retain(|(m, _)| *m != "temp_c");
                        }
                        readings.extend(smart);
                    }
                    Err(e) if !smart_failure_reported => {
                        smart_failure_reported = true;
                        hub.log_event(
                            Severity::Warn,
                            ctrl,
                            &format!(
                                "SMART data unavailable ({e}); wear and error alerts are off. \
                                 healthd ran `doas {}`: add a doas rule that allows exactly \
                                 that (see README).",
                                smart_args(hub.nvme_command(), ctrl).join(" ")
                            ),
                        );
                    }
                    Err(_) => {} // already reported once; retry quietly
                }
            }
            hub.record(ctrl, &readings, true);
        }
        ticker.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Typical `nvme smart-log -o json` output (trimmed).
    const SMART_JSON: &str = r#"{
        "critical_warning": 0, "temperature": 313, "avail_spare": 100,
        "spare_thresh": 10, "percent_used": 7, "media_errors": 0,
        "unsafe_shutdowns": 42, "power_cycles": 900 }"#;

    /// SMART JSON maps to the documented metric names, Kelvin -> Celsius.
    #[test]
    fn smart_json() {
        let r = parse_smart_log(SMART_JSON).unwrap();
        let get = |n: &str| r.iter().find(|(m, _)| *m == n).map(|&(_, v)| v);
        assert_eq!(get("percentage_used"), Some(7.0));
        assert_eq!(get("avail_spare"), Some(100.0));
        assert_eq!(get("media_errors"), Some(0.0));
        assert_eq!(get("critical_warning"), Some(0.0));
        assert_eq!(get("unsafe_shutdowns"), Some(42.0));
        assert!((get("temp_c").unwrap() - 39.85).abs() < 1e-9);
        assert!(parse_smart_log("not json").is_none());
        assert!(parse_smart_log("{}").is_none());
    }

    /// Celsius values and the older key name are handled.
    #[test]
    fn smart_json_variants() {
        let r = parse_smart_log(r#"{"temperature": 41, "percentage_used": 3}"#).unwrap();
        assert!(r.contains(&("percentage_used", 3.0)));
        assert!(r.contains(&("temp_c", 41.0)));
    }

    /// The doas command line is exactly what the README tells users to permit.
    #[test]
    fn smart_command_line() {
        assert_eq!(
            smart_args("nvme", "nvme0").join(" "),
            "-n nvme smart-log /dev/nvme0 -o json"
        );
        assert_eq!(smart_args("/usr/bin/nvme", "nvme1")[1], "/usr/bin/nvme");
    }

    /// Write an executable fake `doas` that runs `body` (a shell snippet).
    fn fake_doas(tag: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("healthd-doas-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doas");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A working helper's JSON comes back; the fake exits non-zero unless it
    /// receives exactly the documented arguments.
    #[test]
    fn smart_runner_success_and_exact_args() {
        let ok = fake_doas(
            "ok",
            r#"[ "$*" = "-n nvme smart-log /dev/nvme0 -o json" ] || exit 9
echo '{"percent_used": 4, "avail_spare": 100, "media_errors": 0}'"#,
        );
        let out = run_smart_log_with(
            ok.to_str().unwrap(),
            "nvme",
            "nvme0",
            Duration::from_secs(5),
        );
        let readings = parse_smart_log(&out.unwrap()).unwrap();
        assert!(readings.contains(&("percentage_used", 4.0)));
        let _ = fs::remove_dir_all(ok.parent().unwrap());
    }

    /// A refusing doas (no matching rule) is an error, not a hang or a panic.
    #[test]
    fn smart_runner_reports_failure_and_timeout() {
        let denied = fake_doas("denied", "exit 1");
        let err = run_smart_log_with(
            denied.to_str().unwrap(),
            "nvme",
            "nvme0",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.contains("failed"), "{err}");

        let hung = fake_doas("hung", "sleep 30");
        let started = Instant::now();
        let err = run_smart_log_with(
            hung.to_str().unwrap(),
            "nvme",
            "nvme0",
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(err, "timed out");
        assert!(started.elapsed() < Duration::from_secs(5));

        let err = run_smart_log_with("/nonexistent/doas", "nvme", "nvme0", Duration::from_secs(1))
            .unwrap_err();
        assert!(err.contains("cannot run"), "{err}");
        for p in [denied, hung] {
            let _ = fs::remove_dir_all(p.parent().unwrap());
        }
    }

    /// Namespace names map to controllers.
    #[test]
    fn controller_names() {
        assert_eq!(controller_of("nvme0n1"), "nvme0");
        assert_eq!(controller_of("nvme12n3"), "nvme12");
        assert_eq!(controller_of("sda"), "sda");
    }

    /// Two namespaces of one controller are reported once.
    #[test]
    fn discovery_dedupes_namespaces() {
        let root = std::env::temp_dir().join(format!("healthd-nvme-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for ns in ["nvme0n1", "nvme0n2", "nvme1n1"] {
            let hw = root.join(ns).join("device").join("hwmon3");
            fs::create_dir_all(&hw).unwrap();
            fs::write(hw.join("temp1_input"), "38000\n").unwrap();
        }
        fs::create_dir_all(root.join("sda")).unwrap();
        let drives = discover(&root);
        assert_eq!(drives.keys().collect::<Vec<_>>(), vec!["nvme0", "nvme1"]);
        assert_eq!(max_celsius(&drives["nvme1"]), Some(38.0));
        let _ = fs::remove_dir_all(&root);
    }
}
