//! Alerting: the threshold-rule [`Engine`] and the desktop [`Notifier`].
//!
//! The engine is pure state (no I/O): feed it observations, get back
//! [`Transition`]s. The hub persists those as events and hands the notifying
//! ones to the notifier, which talks D-Bus on its own thread so a slow or
//! missing notification daemon can never stall a poller.

use std::collections::HashMap;
use std::error::Error;
use std::sync::mpsc;
use std::thread;

use healthd_proto::{format_value, ActiveAlert, Severity};
use notify_rust::{Notification, Timeout, Urgency};

use crate::config::Rule;

/// Stack size of the notifier thread.
///
/// WHY 1 MiB: zbus builds D-Bus messages on the calling thread's stack. Stack
/// is reserved virtual memory, only touched pages count towards RSS.
const NOTIFY_STACK: usize = 1 << 20;

/// Something worth recording (and usually showing) after an observation.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// Severity of the event.
    pub severity: Severity,
    /// Source that caused it.
    pub source: String,
    /// Event text.
    pub msg: String,
    /// Show a desktop notification? `false` for recoveries.
    pub notify: bool,
}

/// Per-(rule, source) bookkeeping.
#[derive(Default)]
struct RuleState {
    /// Consecutive violating observations so far.
    streak: u32,
    /// Is the rule currently in the alerting state?
    active: bool,
    /// Was the current episode announced (so a recovery is worth reporting)?
    announced: bool,
    /// Unix time the current episode began.
    since: i64,
    /// Unix time of the last notification, for the cooldown.
    last_fired: Option<i64>,
    /// Most recent observed value.
    last_value: f64,
}

/// Evaluates rules against a stream of observations.
pub struct Engine {
    rules: Vec<Rule>,
    /// Keyed by (rule index, observed source) so `nvme0` and `nvme1` are
    /// tracked independently even though one rule covers both.
    states: HashMap<(usize, String), RuleState>,
}

impl Engine {
    /// Create an engine for `rules`.
    pub fn new(rules: Vec<Rule>) -> Self {
        Self {
            rules,
            states: HashMap::new(),
        }
    }

    /// Feed one observation; returns any alerts or recoveries it triggers.
    ///
    /// A rule alerts once `sustain` consecutive observations violate it, then
    /// at most once per `cooldown_min` while the violation persists. When the
    /// value returns to normal after an announced alert, an info-level
    /// recovery is emitted (event only, no notification).
    pub fn observe(&mut self, now: i64, source: &str, metric: &str, value: f64) -> Vec<Transition> {
        let mut out = Vec::new();
        for (idx, rule) in self.rules.iter().enumerate() {
            if rule.metric != metric || !rule.matches_source(source) {
                continue;
            }
            let st = self.states.entry((idx, source.to_owned())).or_default();
            st.last_value = value;

            if rule.op.test(value, rule.threshold) {
                st.streak = st.streak.saturating_add(1);
                if st.streak < rule.sustain {
                    continue;
                }
                if !st.active {
                    st.active = true;
                    st.since = now;
                }
                let cooldown = i64::from(rule.cooldown_min) * 60;
                let cooled = match st.last_fired {
                    None => true,
                    Some(t) => now - t >= cooldown,
                };
                if cooled {
                    st.last_fired = Some(now);
                    st.announced = true;
                    out.push(Transition {
                        severity: rule.severity,
                        source: source.to_owned(),
                        msg: rule.render(source, value),
                        notify: true,
                    });
                }
            } else {
                st.streak = 0;
                if st.active {
                    st.active = false;
                    if st.announced {
                        st.announced = false;
                        out.push(Transition {
                            severity: Severity::Info,
                            source: source.to_owned(),
                            msg: format!(
                                "{}: back to normal ({metric} = {})",
                                rule.name,
                                format_value(value)
                            ),
                            notify: false,
                        });
                    }
                }
            }
        }
        out
    }

    /// Rules currently in the alerting state, worst and oldest first.
    pub fn active(&self) -> Vec<ActiveAlert> {
        let mut alerts: Vec<ActiveAlert> = self
            .states
            .iter()
            .filter(|(_, st)| st.active)
            .map(|((idx, source), st)| {
                let rule = &self.rules[*idx];
                ActiveAlert {
                    rule: rule.name.clone(),
                    severity: rule.severity,
                    source: source.clone(),
                    metric: rule.metric.clone(),
                    value: st.last_value,
                    since: st.since,
                    msg: rule.render(source, st.last_value),
                }
            })
            .collect();
        alerts.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.since.cmp(&b.since)));
        alerts
    }
}

/// A queued desktop notification.
struct Notice {
    severity: Severity,
    title: String,
    body: String,
}

/// Sends desktop notifications from a dedicated thread.
pub struct Notifier {
    tx: Option<mpsc::Sender<Notice>>,
}

impl Notifier {
    /// Start the notifier thread, or a no-op notifier when `enabled` is false.
    ///
    /// The thread blocks in `recv()` — zero CPU, and the D-Bus connection is
    /// only opened when the first alert actually fires.
    pub fn start(enabled: bool) -> Self {
        if !enabled {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::channel::<Notice>();
        let spawned = thread::Builder::new()
            .name("notify".to_owned())
            .stack_size(NOTIFY_STACK)
            .spawn(move || {
                for notice in rx {
                    if let Err(e) = show(&notice) {
                        eprintln!("healthd: desktop notification failed: {e}");
                    }
                }
            });
        match spawned {
            Ok(_) => Self { tx: Some(tx) },
            Err(e) => {
                eprintln!("healthd: cannot start notifier thread: {e}");
                Self { tx: None }
            }
        }
    }

    /// Queue a notification; never blocks.
    pub fn send(&self, severity: Severity, source: &str, body: &str) {
        if let Some(tx) = &self.tx {
            // A send error means the thread died; alerts are still in the DB.
            let _ = tx.send(Notice {
                severity,
                title: format!("healthd: {source}"),
                body: body.to_owned(),
            });
        }
    }
}

/// Show one notification over D-Bus.
fn show(notice: &Notice) -> Result<(), Box<dyn Error>> {
    // WHY these timeouts: info fades quickly, warnings stay long enough to be
    // read after glancing away, and critical alerts persist until dismissed.
    let (urgency, icon, timeout) = match notice.severity {
        Severity::Info => (
            Urgency::Low,
            "dialog-information",
            Timeout::Milliseconds(8_000),
        ),
        Severity::Warn => (
            Urgency::Normal,
            "dialog-warning",
            Timeout::Milliseconds(15_000),
        ),
        Severity::Crit => (Urgency::Critical, "dialog-error", Timeout::Never),
    };
    Notification::new()
        .appname("healthd")
        .summary(&notice.title)
        .body(&notice.body)
        .icon(icon)
        .urgency(urgency)
        .timeout(timeout)
        .show()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Engine loaded with the built-in rules.
    fn engine() -> Engine {
        Engine::new(Config::builtin().unwrap().rules)
    }

    /// `cpu-hot` needs 3 consecutive hot samples; a dip resets the streak.
    #[test]
    fn sustain_requires_consecutive_samples() {
        let mut e = engine();
        assert!(e.observe(0, "cpu", "temp_c", 90.0).is_empty());
        assert!(e.observe(5, "cpu", "temp_c", 90.0).is_empty());
        assert!(e.observe(10, "cpu", "temp_c", 70.0).is_empty()); // streak reset
        assert!(e.observe(15, "cpu", "temp_c", 90.0).is_empty());
        assert!(e.observe(20, "cpu", "temp_c", 90.0).is_empty());
        let fired = e.observe(25, "cpu", "temp_c", 90.0);
        assert_eq!(fired.len(), 1);
        assert!(fired[0].notify && fired[0].severity == Severity::Warn);
        assert!(fired[0].msg.contains("90"));
    }

    /// Repeat alerts are held back by the cooldown, then allowed again.
    #[test]
    fn cooldown_limits_repeats() {
        let mut e = engine();
        let t = |e: &mut Engine, now| e.observe(now, "nvme0", "media_errors", 3.0).len();
        assert_eq!(t(&mut e, 0), 1);
        assert_eq!(t(&mut e, 600), 0); // still inside the 24 h cooldown
        assert_eq!(t(&mut e, 1_440 * 60), 1); // cooldown elapsed
    }

    /// Recovery is reported once, silently, and clears the active list.
    #[test]
    fn recovery_event_and_active_list() {
        let mut e = engine();
        e.observe(0, "ram", "available_mb", 100.0);
        let fired = e.observe(5, "ram", "available_mb", 100.0);
        assert_eq!(fired.len(), 1);
        assert_eq!(e.active().len(), 1);
        assert_eq!(e.active()[0].source, "ram");
        let rec = e.observe(10, "ram", "available_mb", 4_000.0);
        assert_eq!(rec.len(), 1);
        assert!(!rec[0].notify && rec[0].severity == Severity::Info);
        assert!(e.active().is_empty());
        assert!(e.observe(15, "ram", "available_mb", 4_000.0).is_empty());
    }

    /// One rule covers many drives, each with its own state.
    #[test]
    fn prefix_rules_track_each_source() {
        let mut e = engine();
        assert_eq!(e.observe(0, "nvme0", "avail_spare", 10.0).len(), 1);
        assert_eq!(e.observe(0, "nvme1", "avail_spare", 10.0).len(), 1);
        assert!(e.observe(1, "nvme2", "avail_spare", 90.0).is_empty());
        assert_eq!(e.active().len(), 2);
    }

    /// Unrelated metrics and sources never trigger anything.
    #[test]
    fn irrelevant_observations_are_ignored() {
        let mut e = engine();
        assert!(e.observe(0, "cpu", "nonexistent", 1e9).is_empty());
        assert!(e.observe(0, "disk", "temp_c", 1e9).is_empty());
    }
}
