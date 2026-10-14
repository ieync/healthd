//! Configuration: an optional TOML file with retention, battery and
//! notification settings plus the alert rules.
//!
//! The built-in default is `config/healthd.toml`, embedded at compile time, so
//! the file users copy and edit is exactly what runs when they have none.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

use healthd_proto::{format_value, Severity};
use serde::Deserialize;

/// The embedded default configuration (also printed by `--print-default-config`).
pub const DEFAULT_TOML: &str = include_str!("../config/healthd.toml");

/// Whole configuration file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// How long data is kept.
    #[serde(default)]
    pub retention: Retention,
    /// Which battery to watch.
    #[serde(default)]
    pub battery: BatteryCfg,
    /// How to invoke `nvme` for SMART data.
    #[serde(default)]
    pub nvme: NvmeCfg,
    /// Desktop notification switch.
    #[serde(default)]
    pub notify: NotifyCfg,
    /// Alert rules (`[[rule]]` tables); replaces the built-in set when present.
    #[serde(default = "builtin_rules", rename = "rule")]
    pub rules: Vec<Rule>,
}

/// Retention and downsampling windows.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    /// Everything older than this many days is deleted.
    pub days: u32,
    /// Samples older than this many hours are averaged into hourly rows.
    pub raw_hours: u32,
}

impl Default for Retention {
    /// 30 days total, 48 h at full resolution (rationale in `config/healthd.toml`).
    fn default() -> Self {
        Self {
            days: 30,
            raw_hours: 48,
        }
    }
}

/// Battery selection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BatteryCfg {
    /// Name under `/sys/class/power_supply/`.
    pub name: String,
}

impl Default for BatteryCfg {
    /// `BAT0`, the most common laptop battery name.
    fn default() -> Self {
        Self {
            name: "BAT0".to_owned(),
        }
    }
}

/// How healthd runs the `nvme` tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NvmeCfg {
    /// Command typed after `doas -n`; must match the `cmd` of the doas rule.
    pub command: String,
}

impl Default for NvmeCfg {
    /// Plain `nvme`, as in the documented `doas -n nvme smart-log`.
    fn default() -> Self {
        Self {
            command: "nvme".to_owned(),
        }
    }
}

/// Notification switch.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyCfg {
    /// Send desktop notifications (events are logged either way).
    pub desktop: bool,
}

impl Default for NotifyCfg {
    /// Notifications on: they are the point of the daemon.
    fn default() -> Self {
        Self { desktop: true }
    }
}

/// Comparison operator of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Op {
    /// `>`
    #[serde(rename = ">")]
    Gt,
    /// `>=`
    #[serde(rename = ">=")]
    Ge,
    /// `<`
    #[serde(rename = "<")]
    Lt,
    /// `<=`
    #[serde(rename = "<=")]
    Le,
}

impl Op {
    /// Does `value <op> threshold` hold?
    pub fn test(self, value: f64, threshold: f64) -> bool {
        match self {
            Self::Gt => value > threshold,
            Self::Ge => value >= threshold,
            Self::Lt => value < threshold,
            Self::Le => value <= threshold,
        }
    }
}

/// One alert rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Unique label shown in events.
    pub name: String,
    /// Source to watch; a trailing `*` makes it a prefix match.
    pub source: String,
    /// Metric to watch.
    pub metric: String,
    /// Comparison operator.
    pub op: Op,
    /// Value compared against.
    pub threshold: f64,
    /// Severity of the alert.
    #[serde(default = "default_severity")]
    pub severity: Severity,
    /// Consecutive violating observations required before alerting.
    #[serde(default = "default_sustain")]
    pub sustain: u32,
    /// Minimum minutes between repeat alerts while violated.
    #[serde(default = "default_cooldown")]
    pub cooldown_min: u32,
    /// Alert text template.
    #[serde(default = "default_message")]
    pub message: String,
}

/// Default severity: `warn`.
fn default_severity() -> Severity {
    Severity::Warn
}

/// Default `sustain`: alert on the first violating observation.
fn default_sustain() -> u32 {
    1
}

/// Default `cooldown_min`: 30 minutes, short enough to re-notice a persistent
/// problem, long enough not to nag.
fn default_cooldown() -> u32 {
    30
}

/// Default message template.
fn default_message() -> String {
    "{source} {metric} is {value} (limit {threshold})".to_owned()
}

impl Rule {
    /// Does this rule watch `source`? Exact match, or prefix match for `name*`.
    pub fn matches_source(&self, source: &str) -> bool {
        match self.source.strip_suffix('*') {
            Some(prefix) => source.starts_with(prefix),
            None => self.source == source,
        }
    }

    /// Fill the message template for an observed `source` and `value`.
    pub fn render(&self, source: &str, value: f64) -> String {
        self.message
            .replace("{source}", source)
            .replace("{metric}", &self.metric)
            .replace("{value}", &format_value(value))
            .replace("{threshold}", &format_value(self.threshold))
    }
}

/// Helper for reading only the rules out of the embedded default file.
#[derive(Deserialize)]
struct RulesOnly {
    /// The `[[rule]]` tables.
    rule: Vec<Rule>,
}

/// Built-in rules, used when the user's file has no `[[rule]]` table.
///
/// Parses `RulesOnly` rather than `Config` to avoid recursing back into this
/// function. A unit test guarantees the embedded file parses.
fn builtin_rules() -> Vec<Rule> {
    toml::from_str::<RulesOnly>(DEFAULT_TOML)
        .map(|r| r.rule)
        .unwrap_or_default()
}

impl Config {
    /// The embedded default configuration.
    pub fn builtin() -> Result<Self, String> {
        toml::from_str(DEFAULT_TOML).map_err(|e| format!("built-in config is invalid: {e}"))
    }

    /// Load the config at `explicit`, or the default path if `None`.
    ///
    /// A missing *default* file silently yields the built-in config; a missing
    /// *explicit* file, or any parse error, is a hard failure — silently
    /// ignoring a typo'd alert rule would be worse than refusing to start.
    pub fn load(explicit: Option<&Path>) -> Result<Self, String> {
        let (path, required) = match explicit {
            Some(p) => (p.to_path_buf(), true),
            None => (default_config_path()?, false),
        };
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !required => {
                return Self::builtin();
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let cfg: Self = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reject settings that would make the daemon misbehave.
    pub fn validate(&self) -> Result<(), String> {
        let r = &self.retention;
        if self.nvme.command.trim().is_empty() {
            return Err("nvme.command must not be empty".to_owned());
        }
        if r.days == 0 || r.raw_hours == 0 {
            return Err("retention.days and retention.raw_hours must be at least 1".to_owned());
        }
        if u64::from(r.raw_hours) > u64::from(r.days) * 24 {
            return Err("retention.raw_hours cannot exceed retention.days * 24".to_owned());
        }
        for rule in &self.rules {
            let name = &rule.name;
            if name.is_empty() || rule.source.is_empty() || rule.metric.is_empty() {
                return Err("every rule needs a non-empty name, source and metric".to_owned());
            }
            if rule.sustain == 0 {
                return Err(format!("rule {name}: sustain must be at least 1"));
            }
            if !rule.threshold.is_finite() {
                return Err(format!("rule {name}: threshold must be a finite number"));
            }
        }
        Ok(())
    }
}

/// `$HOME`, or an error message.
fn home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "$HOME is not set".to_owned())
}

/// XDG base directory lookup: `$var` if absolute, else `$HOME/fallback`.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf, String> {
    match env::var_os(var).map(PathBuf::from) {
        // The XDG spec says relative values must be ignored.
        Some(p) if p.is_absolute() => Ok(p),
        _ => Ok(home()?.join(fallback)),
    }
}

/// `$XDG_CONFIG_HOME/healthd/config.toml` (default `~/.config/...`).
pub fn default_config_path() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_CONFIG_HOME", ".config")?.join("healthd/config.toml"))
}

/// `$XDG_DATA_HOME/healthd/health.db` (default `~/.local/share/healthd/health.db`).
pub fn default_db_path() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_DATA_HOME", ".local/share")?.join("healthd/health.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded default parses, validates and contains the requested rules.
    #[test]
    fn builtin_config_is_valid() {
        let cfg = Config::builtin().unwrap();
        cfg.validate().unwrap();
        let find = |name: &str| cfg.rules.iter().find(|r| r.name == name).unwrap();
        assert_eq!(find("cpu-hot").threshold, 85.0);
        assert_eq!(find("nvme-spare-low").threshold, 20.0);
        assert_eq!(find("nvme-media-errors").threshold, 0.0);
        assert_eq!(find("nvme-hot").threshold, 75.0);
        assert_eq!(find("battery-worn").threshold, 80.0);
        assert_eq!(find("ram-low").threshold, 500.0);
        assert_eq!(builtin_rules().len(), cfg.rules.len());
        assert_eq!(cfg.nvme.command, "nvme");
    }

    /// A file without `[[rule]]` falls back to the built-in rules.
    #[test]
    fn missing_rules_use_builtin() {
        let cfg: Config = toml::from_str("[retention]\ndays = 7\nraw_hours = 12\n").unwrap();
        assert_eq!(cfg.retention.days, 7);
        assert!(!cfg.rules.is_empty());
    }

    /// Unknown keys are errors, so typos are caught at startup.
    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("[retention]\ndayz = 1\n").is_err());
    }

    /// Prefix sources and templates behave.
    #[test]
    fn rule_matching_and_rendering() {
        let cfg = Config::builtin().unwrap();
        let rule = cfg.rules.iter().find(|r| r.name == "nvme-hot").unwrap();
        assert!(rule.matches_source("nvme0") && rule.matches_source("nvme12"));
        assert!(!rule.matches_source("cpu"));
        assert_eq!(
            rule.render("nvme1", 78.0),
            "nvme1 is at 78 C (limit 75 C). Check heatsink and airflow."
        );
    }

    /// Operators compare correctly at the boundary.
    #[test]
    fn operators() {
        assert!(Op::Gt.test(2.0, 1.0) && !Op::Gt.test(1.0, 1.0));
        assert!(Op::Ge.test(1.0, 1.0));
        assert!(Op::Lt.test(0.0, 1.0) && !Op::Lt.test(1.0, 1.0));
        assert!(Op::Le.test(1.0, 1.0));
    }

    /// Validation catches nonsense retention and sustain values.
    #[test]
    fn validation() {
        let mut cfg = Config::builtin().unwrap();
        cfg.retention.raw_hours = 24 * 31;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::builtin().unwrap();
        cfg.rules[0].sustain = 0;
        assert!(cfg.validate().is_err());
    }
}
