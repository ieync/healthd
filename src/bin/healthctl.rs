//! `healthctl` — command-line frontend for the healthd socket API.
//!
//! Talks to the daemon only through `healthd-proto` (no SQLite). Every
//! sub-command has a human-readable form and a `--json` form that prints the
//! daemon's `data` payload unchanged, so it can be piped into `jq` or a status
//! bar script.
//!
//! ```text
//! healthctl status
//! healthctl history cpu temp_c --since 6h
//! healthctl events --since 7d --min-severity warn
//! ```

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use healthd_proto::{
    default_socket_path, format_duration, format_ts, format_with_unit, now_ts, Client, Point,
    Request, Severity,
};

/// Width of the history sparkline in characters.
///
/// WHY 60: fits an 80-column terminal next to the label and summary.
const SPARK_WIDTH: usize = 60;

/// Sparkline glyphs, lowest to highest.
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Top-level command line.
#[derive(Parser)]
#[command(version, about = "Query the healthd system health daemon")]
struct Cli {
    /// Daemon socket (default: $HEALTHD_SOCK or ~/.run/healthd.sock)
    #[arg(long, global = true, value_name = "FILE")]
    socket: Option<PathBuf>,
    /// Print the daemon's raw JSON payload instead of a formatted view
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Cmd,
}

/// Sub-commands, one per API command.
#[derive(Subcommand)]
enum Cmd {
    /// Latest value of every metric and any active alerts
    Status,
    /// Stored samples of one metric
    History(HistoryArgs),
    /// Recent events (alerts, recoveries, diagnostics)
    Events(EventsArgs),
}

/// Arguments of `history`.
#[derive(Args)]
struct HistoryArgs {
    /// Source, e.g. cpu, ram, battery, nvme0
    source: String,
    /// Metric, e.g. temp_c, load_pct, health_pct
    metric: String,
    // WHY 24h / 1000: the same defaults the daemon applies itself. A day is the
    // usual "what happened lately" question, and 1000 points is already more
    // than the sparkline (SPARK_WIDTH columns) can show.
    /// How far back: 90s, 30m, 6h, 7d, 2w — or an absolute Unix timestamp
    #[arg(long, default_value = "24h", value_parser = parse_since)]
    since: i64,
    /// Maximum number of points (newest are kept)
    #[arg(long, default_value_t = 1000)]
    limit: u32,
    /// Print one `timestamp<TAB>value` line per point
    #[arg(long, conflicts_with = "json")]
    raw: bool,
}

/// Arguments of `events`.
#[derive(Args)]
struct EventsArgs {
    // WHY 24h / 100: the daemon's own defaults; a day of alerts is a handful of
    // lines, and 100 keeps a flapping rule from flooding the terminal.
    /// How far back: 90s, 30m, 6h, 7d, 2w — or an absolute Unix timestamp
    #[arg(long, default_value = "24h", value_parser = parse_since)]
    since: i64,
    /// Maximum number of events (newest are kept)
    #[arg(long, default_value_t = 100)]
    limit: u32,
    /// Hide events below this severity: info, warn or crit
    #[arg(long, value_parser = parse_severity)]
    min_severity: Option<Severity>,
}

/// Parse a severity name for clap.
fn parse_severity(s: &str) -> Result<Severity, String> {
    Severity::parse(s).ok_or_else(|| format!("'{s}' is not one of: info, warn, crit"))
}

/// Parse `--since`: a relative span (`90s`, `30m`, `6h`, `7d`, `2w`) becomes an
/// absolute Unix time; a bare number of at least ten digits is taken as a Unix
/// timestamp already.
fn parse_since(s: &str) -> Result<i64, String> {
    parse_since_at(s, now_ts())
}

/// [`parse_since`] with an explicit "now", so it can be tested.
fn parse_since_at(s: &str, now: i64) -> Result<i64, String> {
    let bad = || format!("'{s}' is not a span like 30m, 6h, 7d or a Unix timestamp");
    let s = s.trim();
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    let n: i64 = num.parse().map_err(|_| bad())?;
    let unit_secs = match unit {
        // WHY 10 digits: that is every timestamp since 2001; shorter bare
        // numbers are almost certainly a forgotten unit, so reject them.
        "" if num.len() >= 10 => return Ok(n),
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return Err(bad()),
    };
    n.checked_mul(unit_secs)
        .map(|span| now - span)
        .ok_or_else(bad)
}

/// ANSI styling, only when writing to a terminal that wants colour.
struct Style {
    on: bool,
}

impl Style {
    /// Honour `NO_COLOR` (https://no-color.org) and non-terminal stdout.
    fn detect() -> Self {
        Self {
            on: io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    /// Wrap `text` in the ANSI code `code` when colour is on.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// Severity label coloured red / yellow / dim, padded to 4 columns.
    fn severity(&self, s: Severity) -> String {
        let code = match s {
            Severity::Crit => "1;31",
            Severity::Warn => "33",
            Severity::Info => "2",
        };
        self.paint(code, &format!("{:<4}", s.as_str()))
    }
}

/// Process entry: errors become a message on stderr and exit status 1.
fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        // A closed pipe (`healthctl ... | head`) is not an error worth reporting.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("healthctl: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Connect, run the chosen command and print the result.
fn run(cli: &Cli) -> io::Result<()> {
    let path = cli.socket.clone().unwrap_or_else(default_socket_path);
    let mut client = Client::connect(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot connect to healthd at {}: {e} (is the daemon running? try `dinitctl start healthd`)",
                path.display()
            ),
        )
    })?;
    let style = Style::detect();
    let mut out = io::stdout().lock();

    match &cli.command {
        Cmd::Status => {
            if cli.json {
                return print_json(&mut out, &client.call(&Request::Status).map_err(to_io)?);
            }
            let status = client.status().map_err(to_io)?;
            print_status(&mut out, &style, &status)
        }
        Cmd::History(a) => {
            let req = Request::History {
                source: a.source.clone(),
                metric: a.metric.clone(),
                since: Some(a.since),
                limit: Some(a.limit),
            };
            if cli.json {
                return print_json(&mut out, &client.call(&req).map_err(to_io)?);
            }
            let data = client
                .history(&a.source, &a.metric, Some(a.since), Some(a.limit))
                .map_err(to_io)?;
            if a.raw {
                for p in &data.points {
                    writeln!(out, "{}\t{}", p.ts, p.value)?;
                }
                return Ok(());
            }
            print_history(&mut out, &a.source, &a.metric, &data.points)
        }
        Cmd::Events(a) => {
            let req = Request::Events {
                since: Some(a.since),
                limit: Some(a.limit),
                min_severity: a.min_severity,
            };
            if cli.json {
                return print_json(&mut out, &client.call(&req).map_err(to_io)?);
            }
            let data = client
                .events(Some(a.since), Some(a.limit), a.min_severity)
                .map_err(to_io)?;
            if data.events.is_empty() {
                return writeln!(out, "no events in that window");
            }
            for e in &data.events {
                writeln!(
                    out,
                    "{}  {}  {:<8} {}",
                    format_ts(e.ts),
                    style.severity(e.severity),
                    e.source,
                    e.msg
                )?;
            }
            Ok(())
        }
    }
}

/// Convert a protocol error into an `io::Error` (keeping `BrokenPipe` intact).
fn to_io(e: healthd_proto::ClientError) -> io::Error {
    match e {
        healthd_proto::ClientError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// Pretty-print a JSON payload followed by a newline.
fn print_json(out: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    writeln!(out, "{value:#}")
}

/// Human-readable `status`: alerts first, then metrics grouped by source.
fn print_status(
    out: &mut impl Write,
    style: &Style,
    s: &healthd_proto::StatusData,
) -> io::Result<()> {
    writeln!(
        out,
        "healthd {} · up {}\n",
        s.version,
        format_duration(s.uptime_secs)
    )?;
    if s.active_alerts.is_empty() {
        writeln!(out, "{}\n", style.paint("32", "No active alerts."))?;
    } else {
        writeln!(out, "{}", style.paint("1", "Active alerts"))?;
        for a in &s.active_alerts {
            writeln!(
                out,
                "  {}  {:<8} {}  (since {})",
                style.severity(a.severity),
                a.source,
                a.msg,
                format_ts(a.since)
            )?;
        }
        writeln!(out)?;
    }
    let mut current = "";
    for m in &s.metrics {
        if m.source != current {
            current = &m.source;
            writeln!(out, "{}", style.paint("1", current))?;
        }
        writeln!(
            out,
            "  {:<18} {:>10}",
            m.metric,
            format_with_unit(&m.metric, m.value)
        )?;
    }
    Ok(())
}

/// Human-readable `history`: a summary line and a sparkline.
fn print_history(
    out: &mut impl Write,
    source: &str,
    metric: &str,
    points: &[Point],
) -> io::Result<()> {
    if points.is_empty() {
        return writeln!(
            out,
            "no stored samples for {source} {metric} in that window \
             (some metrics, e.g. all of `ram`, are live-only; see `healthctl status`)"
        );
    }
    let values: Vec<f64> = points.iter().map(|p| p.value).collect();
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let avg = values.iter().sum::<f64>() / values.len() as f64;
    let show = |v: f64| format_with_unit(metric, v);
    writeln!(
        out,
        "{source} {metric}: {} points, {} .. {}\n  min {}  avg {}  max {}  last {}\n  {}",
        points.len(),
        format_ts(points[0].ts),
        format_ts(points[points.len() - 1].ts),
        show(min),
        show(avg),
        show(max),
        show(values[values.len() - 1]),
        sparkline(&values, SPARK_WIDTH),
    )
}

/// Render `values` as a sparkline at most `width` characters wide.
///
/// Points are averaged into `width` equal buckets first, so a 10 000-point
/// history still fits one terminal line.
fn sparkline(values: &[f64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let buckets = width.min(values.len());
    let cols: Vec<f64> = (0..buckets)
        .map(|i| {
            let chunk = &values[i * values.len() / buckets..(i + 1) * values.len() / buckets];
            chunk.iter().sum::<f64>() / chunk.len() as f64
        })
        .collect();
    let min = cols.iter().copied().fold(f64::INFINITY, f64::min);
    let max = cols.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    cols.iter()
        .map(|&v| {
            if span <= f64::EPSILON {
                // A flat line is drawn mid-height rather than as a floor.
                SPARK_GLYPHS[SPARK_GLYPHS.len() / 2]
            } else {
                let idx = ((v - min) / span * (SPARK_GLYPHS.len() - 1) as f64).round() as usize;
                SPARK_GLYPHS[idx.min(SPARK_GLYPHS.len() - 1)]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Relative spans become absolute times; timestamps pass through.
    #[test]
    fn since_parsing() {
        assert_eq!(parse_since_at("30m", 10_000), Ok(10_000 - 1_800));
        assert_eq!(parse_since_at("6h", 100_000), Ok(100_000 - 21_600));
        assert_eq!(parse_since_at("2w", 2_000_000), Ok(2_000_000 - 1_209_600));
        assert_eq!(parse_since_at("1760000000", 1), Ok(1_760_000_000));
        assert!(parse_since_at("90", 1).is_err()); // forgotten unit
        assert!(parse_since_at("h", 1).is_err());
        assert!(parse_since_at("5x", 1).is_err());
        assert!(parse_since_at("", 1).is_err());
        assert!(parse_since_at("99999999999999999999d", 1).is_err());
    }

    /// Sparklines scale to the data and respect the width.
    #[test]
    fn sparkline_shapes() {
        assert_eq!(sparkline(&[1.0, 2.0, 3.0, 4.0], 4), "▁▃▆█");
        assert_eq!(sparkline(&[5.0, 5.0], 10), "▅▅");
        assert_eq!(sparkline(&[], 10), "");
        let long: Vec<f64> = (0..1_000).map(f64::from).collect();
        assert_eq!(sparkline(&long, 60).chars().count(), 60);
    }

    /// Severity names parse for clap.
    #[test]
    fn severity_parsing() {
        assert_eq!(parse_severity("warn"), Ok(Severity::Warn));
        assert!(parse_severity("loud").is_err());
    }

    /// Status output lists alerts, groups by source and applies units.
    #[test]
    fn status_rendering() {
        use healthd_proto::{ActiveAlert, Metric, StatusData};
        let status = StatusData {
            version: "0.1.0".into(),
            uptime_secs: 3 * 3_600 + 5 * 60,
            metrics: vec![
                Metric {
                    source: "cpu".into(),
                    metric: "temp_c".into(),
                    value: 61.5,
                    ts: 0,
                },
                Metric {
                    source: "cpu".into(),
                    metric: "load_pct".into(),
                    value: 12.0,
                    ts: 0,
                },
                Metric {
                    source: "ram".into(),
                    metric: "available_mb".into(),
                    value: 8000.0,
                    ts: 0,
                },
            ],
            active_alerts: vec![ActiveAlert {
                rule: "ram-low".into(),
                severity: Severity::Warn,
                source: "ram".into(),
                metric: "available_mb".into(),
                value: 100.0,
                since: 0,
                msg: "Only 100 MB of RAM available".into(),
            }],
            rules: vec![],
        };
        let mut buf = Vec::new();
        print_status(&mut buf, &Style { on: false }, &status).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("up 3h 05m"));
        assert!(text.contains("Active alerts"));
        assert!(text.contains("warn  ram"));
        assert!(text.contains("61.5°C"));
        assert!(text.contains("8000 MB"));
        assert_eq!(text.matches("\ncpu\n").count(), 1); // grouped once
    }

    /// The history summary reports count, min and max with units.
    #[test]
    fn history_summary_mentions_range() {
        let pts = [
            Point { ts: 0, value: 40.0 },
            Point {
                ts: 60,
                value: 60.0,
            },
        ];
        let mut buf = Vec::new();
        print_history(&mut buf, "cpu", "temp_c", &pts).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("2 points"));
        assert!(text.contains(&format!("min {}", format_with_unit("temp_c", 40.0))));
        assert!(text.contains(&format!("max {}", format_with_unit("temp_c", 60.0))));
    }
}
