// Number, unit and time formatting. formatValue, unitFor and formatDuration
// deliberately match healthd-proto (so `healthctl`, `health-tui` and the
// dashboard print the same text); the GB switch is dashboard-only.

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** `-0` / `-0.0` look like a bug on screen; show plain zero. */
function noNegativeZero(s: string): string {
  return /^-0(\.0+)?$/.test(s) ? s.slice(1) : s;
}

/**
 * Compact number: whole numbers and values from 100 up without decimals,
 * everything else with one. WHY 100: from three digits on a decimal is noise
 * (`7408.3`), below it one decimal still matters (`61.5`, `0.4`).
 */
export function formatValue(v: number): string {
  if (!Number.isFinite(v)) return 'n/a';
  return noNegativeZero(Number.isInteger(v) || Math.abs(v) >= 100 ? v.toFixed(0) : v.toFixed(1));
}

/** Display unit (with its separator) by the pollers' metric naming convention. */
export function unitFor(metric: string): string {
  switch (metric) {
    case 'percentage_used':
    case 'avail_spare':
    case 'psi_some_avg10':
      return '%';
    case 'hours_held_full':
      return ' h';
    default:
      if (metric.endsWith('_c')) return '°C';
      if (metric.endsWith('_pct')) return '%';
      if (metric.endsWith('_mb')) return ' MB';
      return '';
  }
}

/** Megabytes from 1000 up read better as gigabytes. */
function asGigabytes(metric: string, v: number): string | null {
  return unitFor(metric) === ' MB' && Math.abs(v) >= 1000 ? `${(v / 1024).toFixed(1)} GB` : null;
}

/** A metric value with its unit, e.g. `61.5°C`, `82%`, `7.4 GB`. */
export function formatWithUnit(metric: string, v: number): string {
  return asGigabytes(metric, v) ?? formatValue(v) + unitFor(metric);
}

/** An axis tick with a fixed number of decimals. */
export function formatTick(metric: string, v: number, decimals: number): string {
  return asGigabytes(metric, v) ?? noNegativeZero(v.toFixed(decimals)) + unitFor(metric);
}

/** `42s`, `7m`, `3h 05m` or `2d 4h`. */
export function formatDuration(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  if (s < 60) return `${s}s`;
  if (s < 3_600) return `${Math.floor(s / 60)}m`;
  if (s < 86_400) return `${Math.floor(s / 3_600)}h ${String(Math.floor((s % 3_600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(s / 86_400)}d ${Math.floor((s % 86_400) / 3_600)}h`;
}

/** `Oct 14, 02:54`, in UTC like the CLI and TUI (the daemon stores Unix seconds). */
export function formatTime(ts: number): string {
  const d = new Date(ts * 1000);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCDate()}, ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`;
}
