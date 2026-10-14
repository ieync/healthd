//! # healthd-proto
//!
//! Everything the daemon and its frontends have in common: the JSONL wire
//! types, a small blocking [`Client`], and a few formatting helpers.
//!
//! Frontends (`healthctl`, `health-tui`, or anything you write) use this crate
//! *only* — never SQLite — so the socket API is the single boundary between
//! "collecting data" and "showing data". (Enforced by convention and review:
//! the two bundled frontends share a Cargo package with the daemon, but never
//! import `rusqlite`.)
//!
//! ## Wire format
//!
//! One JSON object per line in each direction:
//!
//! ```text
//! -> {"cmd":"status"}
//! <- {"ok":true,"data":{...}}
//! -> {"cmd":"history","source":"cpu","metric":"temp_c","since":1760000000,"limit":500}
//! -> {"cmd":"events","since":1760000000,"limit":50,"min_severity":"warn"}
//! <- {"ok":false,"error":"..."}
//! ```

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fmt};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Environment variable that overrides the socket path for daemon and clients.
pub const SOCKET_ENV: &str = "HEALTHD_SOCK";

/// Longest request line the daemon accepts, in bytes.
///
/// WHY 4 KiB: the largest legitimate request is a `history` call with two short
/// names; anything bigger is a bug or abuse and must not grow a buffer.
pub const MAX_LINE: usize = 4096;

/// Client-side socket timeout.
///
/// WHY 5 s: the daemon answers from RAM or a tiny SQLite file in milliseconds;
/// 5 s only trips when the daemon is wedged, and a frontend should say so
/// rather than hang forever.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Default socket location: `$HEALTHD_SOCK`, else `$HOME/.run/healthd.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = env::var_os(SOCKET_ENV).filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".run").join("healthd.sock")
}

/// Current wall-clock time as Unix seconds.
pub fn now_ts() -> i64 {
    // WHY map_or(0): a clock before 1970 means a broken RTC; 0 is harmless.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// How bad an event is; ordered so `Crit > Warn > Info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational (recoveries, diagnostics).
    Info,
    /// Needs attention soon.
    Warn,
    /// Needs attention now.
    Crit,
}

impl Severity {
    /// Lower-case name as stored in SQLite and sent over the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Crit => "crit",
        }
    }

    /// Parse the lower-case name back into a [`Severity`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "crit" => Some(Self::Crit),
            _ => None,
        }
    }
}

impl fmt::Display for Severity {
    /// Formats as the lower-case name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A request line sent by a frontend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Latest value of every metric plus currently active alerts.
    Status,
    /// Stored samples of one metric, oldest first.
    History {
        /// Sample source, e.g. `cpu` or `nvme0`.
        source: String,
        /// Metric name, e.g. `temp_c`.
        metric: String,
        /// Earliest Unix timestamp to return (default: 24 h ago).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        /// Max points to return, newest kept (default 1000, max 10000).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Recent events, oldest first.
    Events {
        /// Earliest Unix timestamp to return (default: 24 h ago).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<i64>,
        /// Max events to return, newest kept (default 100, max 1000).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        /// Hide events below this severity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_severity: Option<Severity>,
    },
}

/// The envelope of every reply: `{ok, data}` or `{ok:false, error}`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// `true` when the command succeeded.
    pub ok: bool,
    /// Command-specific payload (present when `ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// Human-readable failure reason (present when not `ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// Build a success reply from any serialisable payload.
    pub fn success<T: Serialize>(data: &T) -> Self {
        match serde_json::to_value(data) {
            Ok(v) => Self {
                ok: true,
                data: Some(v),
                error: None,
            },
            Err(e) => Self::failure(format!("serialisation failed: {e}")),
        }
    }

    /// Build a failure reply.
    pub fn failure(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

/// Latest reading of one metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Metric {
    /// Sample source, e.g. `cpu`, `nvme0`.
    pub source: String,
    /// Metric name, e.g. `temp_c`.
    pub metric: String,
    /// Most recent value.
    pub value: f64,
    /// Unix time of that value.
    pub ts: i64,
}

/// An alert rule that is currently violated.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveAlert {
    /// Rule name from the config.
    pub rule: String,
    /// Rule severity.
    pub severity: Severity,
    /// Source that violated it (`nvme1`, not the `nvme*` pattern).
    pub source: String,
    /// Metric that violated it.
    pub metric: String,
    /// Latest observed value.
    pub value: f64,
    /// Unix time the violation began.
    pub since: i64,
    /// Rendered alert text.
    pub msg: String,
}

/// Payload of `status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusData {
    /// Daemon version.
    pub version: String,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Latest value of every metric, sorted by source then metric.
    pub metrics: Vec<Metric>,
    /// Currently violated rules, worst first.
    pub active_alerts: Vec<ActiveAlert>,
}

/// One stored sample.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Unix time.
    pub ts: i64,
    /// Value.
    pub value: f64,
}

/// Payload of `history`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryData {
    /// Echo of the requested source.
    pub source: String,
    /// Echo of the requested metric.
    pub metric: String,
    /// Points, oldest first.
    pub points: Vec<Point>,
}

/// One stored event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unix time.
    pub ts: i64,
    /// Severity.
    pub severity: Severity,
    /// Originating source.
    pub source: String,
    /// Message text.
    pub msg: String,
}

/// Payload of `events`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventsData {
    /// Events, oldest first.
    pub events: Vec<Event>,
}

/// Everything that can go wrong talking to the daemon.
#[derive(Debug)]
pub enum ClientError {
    /// Socket I/O failed (daemon down, timeout, ...).
    Io(io::Error),
    /// The reply was not valid JSON of the expected shape.
    Json(serde_json::Error),
    /// The daemon answered `ok:false`.
    Daemon(String),
}

impl fmt::Display for ClientError {
    /// One-line description suitable for a terminal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Json(e) => write!(f, "malformed reply: {e}"),
            Self::Daemon(m) => write!(f, "daemon error: {m}"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    /// Wrap an I/O error.
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ClientError {
    /// Wrap a JSON error.
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

/// A blocking connection to the daemon; may be reused for many requests.
pub struct Client {
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connect to the daemon socket at `path`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Send one request and return the reply's `data` (or the daemon's error).
    pub fn call(&mut self, req: &Request) -> Result<Value, ClientError> {
        let mut line = serde_json::to_string(req)?;
        line.push('\n');
        let stream = self.reader.get_mut();
        stream.write_all(line.as_bytes())?;
        stream.flush()?;

        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
        }
        let resp: Response = serde_json::from_str(&reply)?;
        if resp.ok {
            Ok(resp.data.unwrap_or(Value::Null))
        } else {
            Err(ClientError::Daemon(
                resp.error.unwrap_or_else(|| "unknown error".to_owned()),
            ))
        }
    }

    /// Send a request and decode `data` into `T`.
    fn typed<T: DeserializeOwned>(&mut self, req: &Request) -> Result<T, ClientError> {
        Ok(serde_json::from_value(self.call(req)?)?)
    }

    /// `status`: latest metrics and active alerts.
    pub fn status(&mut self) -> Result<StatusData, ClientError> {
        self.typed(&Request::Status)
    }

    /// `history`: stored samples of one metric.
    pub fn history(
        &mut self,
        source: &str,
        metric: &str,
        since: Option<i64>,
        limit: Option<u32>,
    ) -> Result<HistoryData, ClientError> {
        self.typed(&Request::History {
            source: source.to_owned(),
            metric: metric.to_owned(),
            since,
            limit,
        })
    }

    /// `events`: recent events.
    pub fn events(
        &mut self,
        since: Option<i64>,
        limit: Option<u32>,
        min_severity: Option<Severity>,
    ) -> Result<EventsData, ClientError> {
        self.typed(&Request::Events {
            since,
            limit,
            min_severity,
        })
    }
}

/// Format a metric value compactly: integers and big numbers without decimals,
/// everything else with one.
pub fn format_value(v: f64) -> String {
    if !v.is_finite() {
        return "n/a".to_owned();
    }
    // WHY 100: from three digits up a decimal is visual noise (`7408.3 MB`),
    // while below it one decimal still matters (`61.5°C`, `0.4%`).
    if v.fract() == 0.0 || v.abs() >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Display unit (with its leading separator) for a metric name.
///
/// WHY by name pattern: pollers follow a naming convention (`*_c`, `*_pct`,
/// `*_mb`), so one function serves every frontend and a new metric that follows
/// the convention gets its unit for free. Unknown metrics get no unit.
pub fn unit_for(metric: &str) -> &'static str {
    match metric {
        "percentage_used" | "avail_spare" | "psi_some_avg10" => "%",
        "hours_held_full" => " h",
        m if m.ends_with("_c") => "°C",
        m if m.ends_with("_pct") => "%",
        m if m.ends_with("_mb") => " MB",
        _ => "",
    }
}

/// A metric value with its unit, e.g. `61.5°C` or `8000 MB`.
pub fn format_with_unit(metric: &str, value: f64) -> String {
    format!("{}{}", format_value(value), unit_for(metric))
}

/// Format a span of seconds as `42s`, `7m`, `3h 05m` or `2d 4h`.
pub fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m", secs / 60),
        3_600..=86_399 => format!("{}h {:02}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d {}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// Convert days since 1970-01-01 to a civil (year, month, day).
///
/// Howard Hinnant's `civil_from_days`; used so we need no date crate.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Format a Unix timestamp as ISO-8601 UTC, e.g. `2026-10-13T14:00:00Z`.
///
/// WHY UTC with a visible `Z`: local time needs a tz database (another
/// dependency for a cosmetic gain), and an unmarked UTC time would be misread
/// as local.
pub fn format_ts(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86_400));
    let s = ts.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3_600,
        s % 3_600 / 60,
        s % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests parse from the documented JSON shapes.
    #[test]
    fn request_json_shapes() {
        let r: Request = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert_eq!(r, Request::Status);
        let r: Request =
            serde_json::from_str(r#"{"cmd":"history","source":"cpu","metric":"temp_c"}"#).unwrap();
        assert!(matches!(r, Request::History { since: None, .. }));
        let r: Request =
            serde_json::from_str(r#"{"cmd":"events","limit":5,"min_severity":"warn"}"#).unwrap();
        assert_eq!(
            r,
            Request::Events {
                since: None,
                limit: Some(5),
                min_severity: Some(Severity::Warn)
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"nope"}"#).is_err());
    }

    /// Optional fields vanish from serialised requests.
    #[test]
    fn request_serialises_compactly() {
        let s = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(s, r#"{"cmd":"status"}"#);
    }

    /// Timestamps format to known UTC strings.
    #[test]
    fn timestamp_formatting() {
        assert_eq!(format_ts(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_ts(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_ts(951_782_400), "2000-02-29T00:00:00Z");
    }

    /// Durations and values format as documented.
    #[test]
    fn small_formatters() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(7 * 60), "7m");
        assert_eq!(format_duration(3 * 3_600 + 5 * 60), "3h 05m");
        assert_eq!(format_duration(2 * 86_400 + 4 * 3_600), "2d 4h");
        assert_eq!(format_value(85.0), "85");
        assert_eq!(format_value(54.26), "54.3");
        assert_eq!(format_value(1234.5), "1234");
        assert_eq!(format_value(f64::NAN), "n/a");
    }

    /// Units follow the metric naming convention.
    #[test]
    fn units() {
        assert_eq!(format_with_unit("temp_c", 61.5), "61.5°C");
        assert_eq!(format_with_unit("load_pct", 12.0), "12%");
        assert_eq!(format_with_unit("available_mb", 8000.0), "8000 MB");
        assert_eq!(format_with_unit("hours_held_full", 3.0), "3 h");
        assert_eq!(format_with_unit("avail_spare", 100.0), "100%");
        assert_eq!(format_with_unit("media_errors", 0.0), "0");
    }

    /// Severity ordering and name round-trip.
    #[test]
    fn severity_order_and_names() {
        assert!(Severity::Crit > Severity::Warn && Severity::Warn > Severity::Info);
        for s in [Severity::Info, Severity::Warn, Severity::Crit] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
        }
        assert_eq!(Severity::parse("loud"), None);
    }
}
