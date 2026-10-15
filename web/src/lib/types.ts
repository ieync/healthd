// Wire types of the healthd API. They mirror crates/healthd-proto/src/lib.rs;
// keep the two in step (the JSON field names are the contract).

export type Severity = 'info' | 'warn' | 'crit';

/** Latest reading of one metric. `ts` is Unix seconds. */
export interface Metric {
  source: string;
  metric: string;
  value: number;
  ts: number;
}

/** An alert rule that is violated right now. */
export interface ActiveAlert {
  rule: string;
  severity: Severity;
  source: string;
  metric: string;
  value: number;
  since: number;
  msg: string;
}

/** A configured rule; lets the chart draw the real alert threshold. */
export interface RuleInfo {
  name: string;
  /** Source pattern; a trailing `*` is a prefix match (`nvme*`). */
  source: string;
  metric: string;
  op: '>' | '>=' | '<' | '<=';
  threshold: number;
  severity: Severity;
}

export interface StatusData {
  version: string;
  uptime_secs: number;
  metrics: Metric[];
  active_alerts: ActiveAlert[];
  /** Absent when talking to a daemon older than the dashboard. */
  rules?: RuleInfo[];
}

export interface Point {
  ts: number;
  value: number;
}

export interface HistoryData {
  source: string;
  metric: string;
  points: Point[];
}

export interface DaemonEvent {
  ts: number;
  severity: Severity;
  source: string;
  msg: string;
}

export interface EventsData {
  events: DaemonEvent[];
}
