// Turns the daemon's flat metric list into the dashboard's cards.
//
// The daemon does not know about cards: it reports (source, metric, value).
// Everything about presentation (titles, order, which metric is the headline,
// friendly names) lives here, and anything unknown still gets a sensible card,
// so a new poller shows up in the dashboard without a frontend change.

import { formatWithUnit } from './format';
import type { ActiveAlert, Metric, RuleInfo, Severity } from './types';

/** Status shown next to a card or the page header. `stale` = no live data. */
export type Tone = 'ok' | 'warn' | 'crit' | 'info' | 'stale';

export const TONE_LABEL: Record<Tone, string> = {
  ok: 'Normal',
  warn: 'Warning',
  crit: 'Critical',
  info: 'Info',
  stale: 'Stale',
};

export const SEVERITY_LABEL: Record<Severity, string> = {
  info: 'Info',
  warn: 'Warning',
  crit: 'Critical',
};

const SEVERITY_RANK: Record<Severity, number> = { info: 0, warn: 1, crit: 2 };

/** Worst severity of a list, or null if empty. */
export function worstSeverity(list: Severity[]): Severity | null {
  let worst: Severity | null = null;
  for (const s of list) if (worst === null || SEVERITY_RANK[s] > SEVERITY_RANK[worst]) worst = s;
  return worst;
}

/** Friendly metric names; anything missing falls back to the metric name itself. */
const LABELS: Record<string, string> = {
  temp_c: 'Temperature',
  load_pct: 'Load',
  throttle_events: 'Throttle events',
  available_mb: 'Available',
  oom_headroom_mb: 'OOM headroom',
  swap_used_pct: 'Swap used',
  psi_some_avg10: 'Memory pressure',
  charge_pct: 'Charge',
  health_pct: 'Health vs design',
  cycles: 'Cycles',
  hours_held_full: 'Held at full',
  percentage_used: 'Wear used',
  avail_spare: 'Spare capacity',
  media_errors: 'Media errors',
  critical_warning: 'Critical warning',
  unsafe_shutdowns: 'Unsafe shutdowns',
};

/** `some_metric_name` -> `Some metric name`. */
export function metricLabel(metric: string): string {
  if (LABELS[metric]) return LABELS[metric];
  const spaced = metric.replace(/_/g, ' ');
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

/** How one kind of source is presented. */
interface Kind {
  matches: (source: string) => boolean;
  title: (source: string) => string;
  /** Headline metric: the first of these that the source reports. */
  heroes: string[];
  /** Order of the remaining rows; unlisted metrics follow alphabetically. */
  order: string[];
  /** Metrics not worth a row (they duplicate another). */
  hidden?: string[];
  /** Values are held in RAM only, never stored (see the README). */
  liveOnly?: boolean;
}

const KINDS: Kind[] = [
  {
    matches: (s) => s === 'cpu',
    title: () => 'CPU',
    heroes: ['temp_c', 'load_pct'],
    order: ['load_pct', 'throttle_events'],
  },
  {
    matches: (s) => s === 'ram',
    title: () => 'Memory',
    heroes: ['available_mb'],
    order: ['oom_headroom_mb', 'swap_used_pct', 'psi_some_avg10'],
    liveOnly: true,
  },
  {
    matches: (s) => s === 'battery',
    title: () => 'Battery',
    heroes: ['charge_pct'],
    order: ['health_pct', 'cycles', 'hours_held_full', 'temp_c'],
    // Only emitted while discharging, and always equal to charge_pct then.
    hidden: ['discharging_pct'],
  },
  {
    matches: (s) => /^nvme\d+$/.test(s),
    title: (s) => `NVMe · ${s}`,
    heroes: ['temp_c'],
    order: ['percentage_used', 'avail_spare', 'media_errors', 'critical_warning', 'unsafe_shutdowns'],
  },
];

/** Fallback for a source we have never heard of. */
const GENERIC_KIND: Kind = {
  matches: () => true,
  title: (s) => s,
  heroes: [],
  order: [],
};

export interface RowModel {
  source: string;
  metric: string;
  label: string;
  /** Formatted value with unit. */
  value: string;
  /** Worst active alert on exactly this metric, if any. */
  alert: Severity | null;
}

export interface CardModel {
  source: string;
  title: string;
  tone: Tone;
  hero: RowModel;
  rows: RowModel[];
  /** True when history is never stored for this source. */
  liveOnly: boolean;
}

/** Position of a source in the card order: known kinds first, in table order. */
function kindIndex(source: string): number {
  const i = KINDS.findIndex((k) => k.matches(source));
  return i === -1 ? KINDS.length : i;
}

/** Group metrics by source and shape each group into a card. */
export function buildCards(metrics: Metric[], alerts: ActiveAlert[]): CardModel[] {
  const bySource = new Map<string, Metric[]>();
  for (const m of metrics) {
    const list = bySource.get(m.source);
    if (list) list.push(m);
    else bySource.set(m.source, [m]);
  }

  const sources = [...bySource.keys()].sort((a, b) => kindIndex(a) - kindIndex(b) || a.localeCompare(b));
  return sources.flatMap((source): CardModel[] => {
    const kind = KINDS.find((k) => k.matches(source)) ?? GENERIC_KIND;
    const mine = (bySource.get(source) ?? []).filter((m) => !kind.hidden?.includes(m.metric));
    // Nothing worth showing (everything was hidden): no card at all.
    if (mine.length === 0) return [];
    const byName = new Map(mine.map((m) => [m.metric, m]));
    const heroName = kind.heroes.find((h) => byName.has(h)) ?? mine[0]?.metric;
    const toRow = (m: Metric): RowModel => ({
      source,
      metric: m.metric,
      label: metricLabel(m.metric),
      value: formatWithUnit(m.metric, m.value),
      alert: worstSeverity(
        alerts.filter((a) => a.source === source && a.metric === m.metric).map((a) => a.severity),
      ),
    });

    const known = kind.order.filter((n) => n !== heroName && byName.has(n));
    const rest = mine
      .map((m) => m.metric)
      .filter((n) => n !== heroName && !kind.order.includes(n))
      .sort();
    const hero = byName.get(heroName ?? '') ?? mine[0];
    return [
      {
        source,
        title: kind.title(source),
        tone: toneOf(alerts.filter((a) => a.source === source)),
        hero: toRow(hero),
        rows: [...known, ...rest].map((n) => toRow(byName.get(n) as Metric)),
        liveOnly: kind.liveOnly ?? false,
      },
    ];
  });
}

/** Tone of a set of alerts: the worst severity, or `ok` when there are none. */
export function toneOf(alerts: ActiveAlert[]): Tone {
  return worstSeverity(alerts.map((a) => a.severity)) ?? 'ok';
}

/** Does a rule's source pattern (`cpu`, `nvme*`) cover `source`? Mirrors the daemon. */
export function ruleCovers(pattern: string, source: string): boolean {
  return pattern.endsWith('*') ? source.startsWith(pattern.slice(0, -1)) : pattern === source;
}

/** Rules that watch exactly this metric of this source. */
export function rulesFor(rules: RuleInfo[], source: string, metric: string): RuleInfo[] {
  return rules.filter((r) => r.metric === metric && ruleCovers(r.source, source));
}

/** Is history never stored for this source? */
export function isLiveOnly(source: string): boolean {
  return (KINDS.find((k) => k.matches(source)) ?? GENERIC_KIND).liveOnly ?? false;
}

/** Title of a source's card, e.g. `Memory`. */
export function sourceTitle(source: string): string {
  return (KINDS.find((k) => k.matches(source)) ?? GENERIC_KIND).title(source);
}
