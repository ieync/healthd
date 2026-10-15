//! `health-tui` — full-screen terminal dashboard for healthd.
//!
//! Like `healthctl` it talks to the daemon only through `healthd-proto`.
//! It polls `status` and `events` every couple of seconds, and `history` for
//! the metric highlighted in the table, then redraws. There is no background
//! thread: one loop alternates between waiting for a key press (with a
//! timeout) and refreshing, so the TUI sleeps in `poll(2)` between updates.
//!
//! Keys: `↑/↓` or `j/k` choose a metric, `w` cycles the chart window,
//! `r` refreshes now, `q` / `Esc` / `Ctrl-C` quit.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use healthd_proto::{
    default_socket_path, format_duration, format_ts, format_with_unit, now_ts, ActiveAlert, Client,
    ClientError, Event, Point, Severity, StatusData,
};
use ratatui::crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Chart, Dataset, GraphType, List, ListItem, Paragraph, Row, Table, TableState,
};
use ratatui::{DefaultTerminal, Frame};

/// How often the screen refreshes from the daemon.
///
/// WHY 2 s: cpu and ram update every 5 s inside the daemon, so 2 s shows each
/// new value promptly while the TUI still sleeps >99 % of the time.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// Chart windows the `w` key cycles through: (label, seconds).
///
/// WHY these four: 1 h for "what is happening now", 6 h / 24 h for the day's
/// pattern, 7 d for trends (older data is the daemon's hourly downsample).
const WINDOWS: [(&str, i64); 4] = [
    ("1h", 3_600),
    ("6h", 21_600),
    ("24h", 86_400),
    ("7d", 604_800),
];

/// Most history points requested for the chart.
///
/// WHY 4000: with default retention a 7-day window holds 48 h of 1-minute
/// rows (2 880) plus 5 days of hourly rows (120); the limit keeps the *newest*
/// points, so it must exceed that or the left edge of the chart is cut off.
const HISTORY_LIMIT: u32 = 4_000;

/// How far back the events panel looks, and how many events it fetches.
///
/// WHY 7 d / 100: recent enough to be relevant, small enough for one reply.
const EVENT_LOOKBACK_SECS: i64 = 604_800;
/// See [`EVENT_LOOKBACK_SECS`].
const EVENTS_FETCH: u32 = 100;

/// Most alert lines shown before the panel stops growing.
///
/// WHY 5: more than that is a systemic problem and the list would push the
/// metrics table off small terminals; the header still shows the full count.
const MAX_ALERT_LINES: usize = 5;

/// Command line.
#[derive(Parser)]
#[command(version, about = "Terminal dashboard for the healthd daemon")]
struct Cli {
    /// Daemon socket (default: $HEALTHD_SOCK or ~/.run/healthd.sock)
    #[arg(long, value_name = "FILE")]
    socket: Option<PathBuf>,
}

/// Everything the UI needs to draw a frame.
struct App {
    socket: PathBuf,
    /// Live connection, or `None` while the daemon is unreachable.
    client: Option<Client>,
    /// Last successful `status` reply.
    status: Option<StatusData>,
    /// Last `events` reply, oldest first.
    events: Vec<Event>,
    /// History of the selected metric, oldest first.
    history: Vec<Point>,
    /// Unix time of the last successful refresh (anchors the chart's x axis).
    fetched_at: i64,
    /// Index into [`WINDOWS`].
    window: usize,
    /// Table cursor (an index into `status.metrics`).
    table: TableState,
    /// (source, metric) under the cursor, so the selection survives reordering.
    selected: Option<(String, String)>,
    /// Why the last refresh failed, if it did.
    error: Option<String>,
    /// Set by a key press that wants an immediate refresh.
    refresh_now: bool,
    /// Set to leave the main loop.
    quit: bool,
}

impl App {
    /// A fresh app that has not talked to the daemon yet.
    fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            client: None,
            status: None,
            events: Vec::new(),
            history: Vec::new(),
            fetched_at: now_ts(),
            // WHY index 1 (6 h): long enough to show a trend, short enough that
            // the full-resolution 1-minute data fills the chart.
            window: 1,
            table: TableState::default(),
            selected: None,
            error: None,
            refresh_now: false,
            quit: false,
        }
    }

    /// Fetch everything from the daemon; on failure remember why and, for I/O
    /// errors, drop the connection so the next refresh reconnects.
    fn refresh(&mut self) {
        self.refresh_now = false;
        match self.fetch() {
            Ok(()) => self.error = None,
            Err(e) => {
                if matches!(e, ClientError::Io(_)) {
                    // A dead socket never recovers; reconnect next time.
                    self.client = None;
                }
                self.error = Some(e.to_string());
            }
        }
    }

    /// Connect if needed, then load status, events and the selected history.
    fn fetch(&mut self) -> Result<(), ClientError> {
        if self.client.is_none() {
            self.client = Some(Client::connect(&self.socket)?);
        }
        let Some(client) = self.client.as_mut() else {
            return Ok(());
        };
        let now = now_ts();
        let status = client.status()?;
        let events = client.events(Some(now - EVENT_LOOKBACK_SECS), Some(EVENTS_FETCH), None)?;
        self.fetched_at = now;
        self.status = Some(status);
        self.events = events.events;
        self.reselect();
        self.fetch_history()
    }

    /// Load the chart data for the selected metric.
    fn fetch_history(&mut self) -> Result<(), ClientError> {
        let (Some(client), Some((source, metric))) = (self.client.as_mut(), self.selected.as_ref())
        else {
            self.history.clear();
            return Ok(());
        };
        let since = self.fetched_at - WINDOWS[self.window].1;
        self.history = client
            .history(source, metric, Some(since), Some(HISTORY_LIMIT))?
            .points;
        Ok(())
    }

    /// Like [`refresh`](Self::refresh) but only the chart (cheap; used when the
    /// user moves the cursor or changes the window).
    fn refresh_history(&mut self) {
        if let Err(e) = self.fetch_history() {
            if matches!(e, ClientError::Io(_)) {
                self.client = None;
            }
            self.error = Some(e.to_string());
        }
    }

    /// The metrics list (empty before the first reply).
    fn metrics(&self) -> &[healthd_proto::Metric] {
        self.status.as_ref().map_or(&[], |s| s.metrics.as_slice())
    }

    /// Re-find the selected (source, metric) in a fresh metrics list, falling
    /// back to the first row.
    fn reselect(&mut self) {
        let metrics = self.metrics();
        let found = self.selected.as_ref().and_then(|(s, m)| {
            metrics
                .iter()
                .position(|x| &x.source == s && &x.metric == m)
        });
        let idx = found.or(if metrics.is_empty() { None } else { Some(0) });
        self.selected = idx.map(|i| (metrics[i].source.clone(), metrics[i].metric.clone()));
        self.table.select(idx);
    }

    /// Move the table cursor by `delta` rows (clamped) and reload the chart.
    fn move_selection(&mut self, delta: isize) {
        let len = self.metrics().len();
        if len == 0 {
            return;
        }
        let cur = self.table.selected().unwrap_or(0);
        let idx = cur.saturating_add_signed(delta).min(len - 1);
        self.selected = Some((
            self.metrics()[idx].source.clone(),
            self.metrics()[idx].metric.clone(),
        ));
        self.table.select(Some(idx));
        self.refresh_history();
    }

    /// Handle one key press.
    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-5),
            KeyCode::PageDown => self.move_selection(5),
            KeyCode::Home => self.move_selection(isize::MIN / 2),
            KeyCode::End => self.move_selection(isize::MAX / 2),
            KeyCode::Char('w') | KeyCode::Tab => {
                self.window = (self.window + 1) % WINDOWS.len();
                self.refresh_history();
            }
            KeyCode::Char('r') => self.refresh_now = true,
            _ => {}
        }
    }
}

/// Colour for a severity.
fn severity_color(s: Severity) -> Color {
    match s {
        Severity::Crit => Color::Red,
        Severity::Warn => Color::Yellow,
        Severity::Info => Color::Cyan,
    }
}

/// Worst severity among alerts that match `(source, metric)`.
fn alert_severity(alerts: &[ActiveAlert], source: &str, metric: &str) -> Option<Severity> {
    alerts
        .iter()
        .filter(|a| a.source == source && a.metric == metric)
        .map(|a| a.severity)
        .max()
}

/// Draw one frame.
fn draw(frame: &mut Frame, app: &mut App) {
    let alert_lines = app
        .status
        .as_ref()
        .map_or(1, |s| s.active_alerts.len().clamp(1, MAX_ALERT_LINES));
    let [header, alerts, middle, events, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(alert_lines as u16 + 2),
        Constraint::Min(8),
        Constraint::Length(9),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [table_area, chart_area] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(middle);

    draw_header(frame, header, app);
    draw_alerts(frame, alerts, app);
    draw_metrics(frame, table_area, app);
    draw_chart(frame, chart_area, app);
    draw_events(frame, events, app);
    draw_footer(frame, footer, app);
}

/// Top line: daemon version, uptime and connection state.
fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " healthd ",
        Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
    )];
    match (&app.error, &app.status) {
        (Some(err), _) => spans.push(Span::styled(
            format!(" ○ {err} — retrying every {}s", REFRESH_EVERY.as_secs()),
            Style::new().fg(Color::Red),
        )),
        (None, Some(s)) => spans.push(Span::styled(
            format!(" ● v{} · up {}", s.version, format_duration(s.uptime_secs)),
            Style::new().fg(Color::Green),
        )),
        (None, None) => spans.push(Span::raw(" connecting…")),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Alerts panel; its border colour is the worst active severity.
fn draw_alerts(frame: &mut Frame, area: Rect, app: &App) {
    let alerts: &[ActiveAlert] = app
        .status
        .as_ref()
        .map_or(&[], |s| s.active_alerts.as_slice());
    let colour = alerts
        .iter()
        .map(|a| a.severity)
        .max()
        .map_or(Color::Green, severity_color);
    let block = Block::bordered()
        .title(format!(" Active alerts ({}) ", alerts.len()))
        .border_style(Style::new().fg(colour));
    let lines: Vec<Line> = if alerts.is_empty() {
        vec![Line::styled(
            "✔ no active alerts",
            Style::new().fg(Color::Green),
        )]
    } else {
        alerts
            .iter()
            .take(MAX_ALERT_LINES)
            .map(|a| {
                Line::from(vec![
                    Span::styled(
                        format!("{:<4} ", a.severity),
                        Style::new()
                            .fg(severity_color(a.severity))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("{} ", a.msg)),
                    Span::styled(
                        format!("(since {})", format_ts(a.since)),
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Metrics table: one row per (source, metric), grouped by source, coloured by
/// any alert that is active on that metric.
fn draw_metrics(frame: &mut Frame, area: Rect, app: &mut App) {
    let alerts: &[ActiveAlert] = app
        .status
        .as_ref()
        .map_or(&[], |s| s.active_alerts.as_slice());
    let now = app.fetched_at;
    let mut last_source = "";
    let rows: Vec<Row> = app
        .metrics()
        .iter()
        .map(|m| {
            // Print the source name only on the first row of its group.
            let source = if m.source == last_source {
                ""
            } else {
                m.source.as_str()
            };
            last_source = &m.source;
            let style = match alert_severity(alerts, &m.source, &m.metric) {
                Some(sev) => Style::new()
                    .fg(severity_color(sev))
                    .add_modifier(Modifier::BOLD),
                None => Style::new(),
            };
            Row::new(vec![
                source.to_owned(),
                m.metric.clone(),
                format_with_unit(&m.metric, m.value),
                format_duration(u64::try_from(now - m.ts).unwrap_or(0)),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(8),
            Constraint::Min(14),
            Constraint::Length(10),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(["source", "metric", "value", "age"])
            .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ")
    .block(Block::bordered().title(" Metrics "));
    frame.render_stateful_widget(table, area, &mut app.table);
}

/// History chart of the selected metric.
fn draw_chart(frame: &mut Frame, area: Rect, app: &App) {
    let (label, window_secs) = WINDOWS[app.window];
    let title = match &app.selected {
        Some((s, m)) => format!(" {s} {m} — last {label} "),
        None => " History ".to_owned(),
    };
    let block = Block::bordered().title(title);
    let (Some((_, metric)), false) = (&app.selected, app.history.is_empty()) else {
        let msg = if app.selected.is_none() {
            "waiting for data…"
        } else {
            "no stored samples in this window\n(some metrics, such as all of ram, are live-only)"
        };
        frame.render_widget(Paragraph::new(msg).block(block), area);
        return;
    };

    let since = app.fetched_at - window_secs;
    let data: Vec<(f64, f64)> = app
        .history
        .iter()
        .map(|p| ((p.ts - since).max(0) as f64, p.value))
        .collect();
    let min = data.iter().map(|d| d.1).fold(f64::INFINITY, f64::min);
    let max = data.iter().map(|d| d.1).fold(f64::NEG_INFINITY, f64::max);
    let avg = data.iter().map(|d| d.1).sum::<f64>() / data.len() as f64;
    // WHY padding: a flat series would otherwise sit on the axis and a series
    // touching the bounds would be clipped. A flat series gets ±1 unit.
    let pad = ((max - min) * 0.1).max(1.0);
    let (lo, hi) = (min - pad, max + pad);
    let show = |v: f64| format_with_unit(metric, v);

    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::new().fg(Color::Cyan))
        .data(&data);
    let chart = Chart::new(vec![dataset])
        .block(block.title_bottom(format!(
            " min {}  avg {}  max {}  now {} ",
            show(min),
            show(avg),
            show(max),
            show(app.history.last().map_or(0.0, |p| p.value)),
        )))
        .x_axis(
            Axis::default()
                .bounds([0.0, window_secs as f64])
                .labels([format!("-{label}"), "now".to_owned()]),
        )
        .y_axis(Axis::default().bounds([lo, hi]).labels([
            show(lo),
            show((lo + hi) / 2.0),
            show(hi),
        ]));
    frame.render_widget(chart, area);
}

/// Events panel, newest first, as many as fit.
fn draw_events(frame: &mut Frame, area: Rect, app: &App) {
    let capacity = usize::from(area.height.saturating_sub(2));
    let items: Vec<ListItem> = app
        .events
        .iter()
        .rev()
        .take(capacity)
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", format_ts(e.ts)),
                    Style::new().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("{:<4} ", e.severity),
                    Style::new().fg(severity_color(e.severity)),
                ),
                Span::raw(format!("{:<8} {}", e.source, e.msg)),
            ]))
        })
        .collect();
    let body = if items.is_empty() {
        List::new([ListItem::new("no events in the last 7 days")])
    } else {
        List::new(items)
    };
    frame.render_widget(body.block(Block::bordered().title(" Events (UTC) ")), area);
}

/// Bottom line: key help.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let help = format!(
        " q quit · ↑↓/jk select · w window ({}) · r refresh",
        WINDOWS[app.window].0
    );
    frame.render_widget(
        Paragraph::new(help).style(Style::new().add_modifier(Modifier::DIM)),
        area,
    );
}

/// Main loop: refresh on a timer, redraw, wait for keys in between.
fn run(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut next_refresh = Instant::now();
    while !app.quit {
        if app.refresh_now || Instant::now() >= next_refresh {
            app.refresh();
            next_refresh = Instant::now() + REFRESH_EVERY;
        }
        terminal.draw(|f| draw(f, app))?;
        // Sleep until a key arrives or the next refresh is due. Terminal resize
        // events also wake us, and the redraw above then adapts to the new size.
        if event::poll(next_refresh.saturating_duration_since(Instant::now()))? {
            if let TermEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key);
                }
            }
        }
    }
    Ok(())
}

/// Entry point: set up the terminal, run, and always restore it.
fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let mut app = App::new(cli.socket.unwrap_or_else(default_socket_path));
    // `init` also installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use healthd_proto::Metric;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// An app pre-filled with plausible data, as if refreshed once.
    fn sample_app() -> App {
        let m = |s: &str, n: &str, v: f64| Metric {
            source: s.into(),
            metric: n.into(),
            value: v,
            ts: 1_000,
        };
        let mut app = App::new(PathBuf::from("/nonexistent"));
        app.fetched_at = 1_000;
        app.status = Some(StatusData {
            version: "0.1.0".into(),
            uptime_secs: 7_500,
            metrics: vec![
                m("cpu", "load_pct", 12.0),
                m("cpu", "temp_c", 91.0),
                m("ram", "available_mb", 300.0),
            ],
            active_alerts: vec![ActiveAlert {
                rule: "cpu-hot".into(),
                severity: Severity::Warn,
                source: "cpu".into(),
                metric: "temp_c".into(),
                value: 91.0,
                since: 900,
                msg: "CPU is at 91 C".into(),
            }],
            rules: vec![],
        });
        app.events = vec![Event {
            ts: 900,
            severity: Severity::Warn,
            source: "cpu".into(),
            msg: "CPU is at 91 C".into(),
        }];
        app.history = (0..50)
            .map(|i| Point {
                ts: 1_000 - 3_000 + i * 60,
                value: 50.0 + i as f64,
            })
            .collect();
        app.reselect();
        app
    }

    /// Render `app` into a text grid of the given size.
    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A full frame shows every panel's key content.
    #[test]
    fn frame_contains_all_panels() {
        let mut app = sample_app();
        let text = render(&mut app, 110, 34);
        for needle in [
            "healthd",
            "up 2h 05m",
            "Active alerts (1)",
            "CPU is at 91 C",
            "Metrics",
            "temp_c",
            "91°C",
            "cpu temp_c",
            "min ",
            "Events (UTC)",
            "q quit",
        ] {
            // The selection starts on the first row (cpu load_pct), so the chart
            // title says load_pct; check the rest independently.
            if needle == "cpu temp_c" {
                continue;
            }
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(text.contains("cpu load_pct"), "chart title in:\n{text}");
    }

    /// Cursor movement is clamped and keeps the selected key in sync.
    #[test]
    fn selection_moves_and_clamps() {
        let mut app = sample_app();
        app.move_selection(-1);
        assert_eq!(app.table.selected(), Some(0));
        app.move_selection(1);
        assert_eq!(app.selected, Some(("cpu".into(), "temp_c".into())));
        app.move_selection(99);
        assert_eq!(app.selected, Some(("ram".into(), "available_mb".into())));
        app.move_selection(isize::MIN / 2);
        assert_eq!(app.table.selected(), Some(0));
    }

    /// Selection survives a reorder of the metrics list.
    #[test]
    fn selection_follows_key_not_index() {
        let mut app = sample_app();
        app.selected = Some(("ram".into(), "available_mb".into()));
        if let Some(s) = app.status.as_mut() {
            s.metrics.reverse();
        }
        app.reselect();
        assert_eq!(app.table.selected(), Some(0));
    }

    /// Disconnected state shows the error and never panics, even tiny.
    #[test]
    fn disconnected_and_tiny_terminals() {
        let mut app = App::new(PathBuf::from("/nonexistent"));
        app.refresh();
        assert!(app.error.is_some() && app.client.is_none());
        let text = render(&mut app, 100, 30);
        assert!(text.contains("retrying"), "{text}");
        for (w, h) in [(1, 1), (10, 3), (20, 8), (60, 12)] {
            let mut full = sample_app();
            render(&mut full, w, h);
        }
    }

    /// Keys map to actions.
    #[test]
    fn key_handling() {
        let mut app = sample_app();
        let press = |c| KeyEvent::new(c, KeyModifiers::NONE);
        app.on_key(press(KeyCode::Char('r')));
        assert!(app.refresh_now);
        let before = app.window;
        app.on_key(press(KeyCode::Char('w')));
        assert_eq!(app.window, (before + 1) % WINDOWS.len());
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);
    }

    /// Alerts colour only their own metric.
    #[test]
    fn alert_lookup() {
        let app = sample_app();
        let alerts = &app.status.as_ref().unwrap().active_alerts;
        assert_eq!(
            alert_severity(alerts, "cpu", "temp_c"),
            Some(Severity::Warn)
        );
        assert_eq!(alert_severity(alerts, "cpu", "load_pct"), None);
    }
}
