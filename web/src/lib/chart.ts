// Chart maths, kept free of Svelte so it can be unit-tested.
//
// Charts are drawn in a 1000 x 100 SVG box that is stretched to fit
// (preserveAspectRatio="none") with a non-scaling stroke, so text and dots live
// in HTML on top and never get distorted. Positions are therefore handed back
// as percentages.

import type { Point } from './types';

/** Width of the SVG coordinate box. */
export const VIEW_W = 1000;
/** Height of the SVG coordinate box. */
export const VIEW_H = 100;

/**
 * Shortest interval that may be drawn as a gap.
 *
 * WHY 600 s: every stored series gets a sample a minute, so ten minutes with
 * nothing means the daemon was stopped or the machine was asleep. A gap is
 * shown as a gap rather than a straight line through data that does not exist.
 */
const GAP_MIN_SECS = 600;

/** An interval is a gap when it is this many times longer than its neighbours.
 *  WHY 3: tolerates a missed sample or two without breaking the line. Older
 *  data is downsampled to hourly rows, and comparing with the neighbours (not a
 *  fixed cadence) keeps those from all looking like gaps. */
const GAP_FACTOR = 3;

export interface Plot {
  /** SVG path data of the line (several sub-paths when there are gaps). */
  line: string;
  /** SVG path data of the area fill under the line. */
  area: string;
  /** Horizontal position of each point, 0..100 (% of the plot width). */
  xs: number[];
  /** Vertical position of each point, 0..100 (% of the plot height, top = 0). */
  ys: number[];
}

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));
const fixed = (n: number) => n.toFixed(1);

/** Indices at which a new, disconnected segment starts (always includes 0). */
function segmentStarts(points: Point[]): number[] {
  const starts = [0];
  for (let i = 1; i < points.length; i++) {
    const dt = points[i].ts - points[i - 1].ts;
    const before = i >= 2 ? points[i - 1].ts - points[i - 2].ts : 0;
    const after = i + 1 < points.length ? points[i + 1].ts - points[i].ts : 0;
    const local = Math.max(before, after);
    if (dt > Math.max(GAP_MIN_SECS, GAP_FACTOR * local)) starts.push(i);
  }
  return starts;
}

/** Map points onto the plot box for the time range `[t0, t1]` and value range `[lo, hi]`. */
export function buildPlot(points: Point[], t0: number, t1: number, lo: number, hi: number): Plot {
  const spanT = t1 - t0 || 1;
  const spanV = hi - lo || 1;
  const xs = points.map((p) => clamp(((p.ts - t0) / spanT) * 100, 0, 100));
  const ys = points.map((p) => clamp(100 - ((p.value - lo) / spanV) * 100, 0, 100));

  const starts = segmentStarts(points);
  const line: string[] = [];
  const area: string[] = [];
  starts.forEach((from, s) => {
    const to = s + 1 < starts.length ? starts[s + 1] : points.length;
    const pts = [];
    for (let i = from; i < to; i++) pts.push(`${fixed(xs[i] * (VIEW_W / 100))} ${fixed(ys[i] * (VIEW_H / 100))}`);
    if (pts.length === 1) {
      // A lone sample: a zero-length stroke, which the round line cap draws as a dot.
      line.push(`M${pts[0]} l0 0`);
      return;
    }
    line.push(`M${pts.join(' L')}`);
    const x0 = fixed(xs[from] * (VIEW_W / 100));
    const xn = fixed(xs[to - 1] * (VIEW_W / 100));
    area.push(`M${x0} ${VIEW_H} L${pts.join(' L')} L${xn} ${VIEW_H} Z`);
  });
  return { line: line.join(' '), area: area.join(' '), xs, ys };
}

/** Index of the point whose time is closest to `ts` (points must be sorted); -1 if empty. */
export function nearestIndex(points: Point[], ts: number): number {
  if (points.length === 0) return -1;
  let lo = 0;
  let hi = points.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (points[mid].ts < ts) lo = mid + 1;
    else hi = mid;
  }
  if (lo > 0 && ts - points[lo - 1].ts <= points[lo].ts - ts) return lo - 1;
  return lo;
}

/** Up to `n` evenly spaced points, always including the first and the last. */
export function sampleRows(points: Point[], n: number): Point[] {
  if (points.length <= n) return points;
  const out: Point[] = [];
  for (let i = 0; i < n; i++) out.push(points[Math.round((i * (points.length - 1)) / (n - 1))]);
  return out;
}

export interface Scale {
  lo: number;
  hi: number;
  /** Three tick values, top to bottom: `hi`, middle, `lo`. */
  ticks: [number, number, number];
  /** Decimals needed to print the ticks exactly. */
  decimals: number;
}

/** Candidate step sizes, ascending, around `raw`: 1, 2, 2.5, 5 times a power of ten. */
function niceSteps(raw: number): number[] {
  const exp = Math.floor(Math.log10(raw));
  const out: number[] = [];
  for (let k = -1; k <= 2; k++) {
    for (const m of [1, 2, 2.5, 5]) out.push(m * 10 ** (exp + k));
  }
  return out.filter((s) => s >= raw * 0.999).sort((a, b) => a - b);
}

/** Fewest decimals that print `step` exactly (2.5 -> 1, 5 -> 0, 0.25 -> 2). */
function decimalsOf(step: number): number {
  for (let d = 0; d < 6; d++) {
    const scaled = step * 10 ** d;
    if (Math.abs(scaled - Math.round(scaled)) < 1e-9) return d;
  }
  return 6;
}

/**
 * Round axis bounds with three ticks (top, middle, bottom).
 *
 * `values` are the data; `extras` are lines that must stay on the chart (alert
 * thresholds). Data that has never been negative never gets a negative axis,
 * and a percentage never gets one above 100, so a flat `100 %` series does not
 * claim to be `101 %`.
 */
export function niceScale(values: number[], extras: number[], percent: boolean): Scale {
  const all = [...values, ...extras];
  let lo = Math.min(...all);
  let hi = Math.max(...all);
  // WHY pad 10 %, at least 1 unit: a flat series would otherwise sit on the
  // axis, and a series touching the bounds would be clipped by its own stroke.
  const pad = Math.max((hi - lo) * 0.1, 1);
  const nonNegative = lo >= 0;
  lo -= pad;
  hi += pad;
  if (nonNegative) lo = Math.max(lo, 0);
  if (percent) hi = Math.min(hi, 100);

  for (const step of niceSteps((hi - lo) / 2)) {
    let bottom = Math.floor(lo / step + 1e-9) * step;
    let top = bottom + 2 * step;
    if (top < hi - 1e-9) continue; // this step is too small to span the data
    if (percent && top > 100) {
      top = 100;
      bottom = Math.max(100 - 2 * step, nonNegative ? 0 : -Infinity);
    }
    const d = decimalsOf(step);
    const round = (n: number) => Number(n.toFixed(d + 2));
    return { lo: round(bottom), hi: round(top), ticks: [round(top), round((top + bottom) / 2), round(bottom)], decimals: d };
  }
  return { lo, hi, ticks: [hi, (lo + hi) / 2, lo], decimals: 1 };
}
