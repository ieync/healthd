import { describe, expect, it } from 'vitest';
import { buildPlot, nearestIndex, niceScale, sampleRows } from './chart';
import type { Point } from './types';

const pts = (...pairs: [number, number][]): Point[] => pairs.map(([ts, value]) => ({ ts, value }));

describe('buildPlot', () => {
  it('maps time and value onto the 1000 x 100 box', () => {
    const p = buildPlot(pts([0, 0], [100, 100]), 0, 100, 0, 100);
    expect(p.xs).toEqual([0, 100]);
    expect(p.ys).toEqual([100, 0]);
    expect(p.line).toBe('M0.0 100.0 L1000.0 0.0');
    expect(p.area.startsWith('M0.0 100 L')).toBe(true);
    expect(p.area.endsWith('Z')).toBe(true);
  });

  it('places points by time, not by index', () => {
    const p = buildPlot(pts([0, 1], [10, 1], [100, 1]), 0, 100, 0, 2);
    expect(p.xs).toEqual([0, 10, 100]);
  });

  it('clamps values outside the range to the box', () => {
    const p = buildPlot(pts([0, -50], [10, 500]), 0, 10, 0, 100);
    expect(p.ys).toEqual([100, 0]);
  });

  it('breaks the line where the data has a gap', () => {
    // A sample a minute, then an hour of nothing, then samples again.
    const p = buildPlot(pts([0, 1], [60, 1], [120, 1], [3_720, 1], [3_780, 1]), 0, 3_780, 0, 2);
    expect(p.line.match(/M/g)).toHaveLength(2);
    expect(p.area.match(/M/g)).toHaveLength(2);
  });

  it('does not mistake evenly spaced hourly rows for gaps', () => {
    const hourly = pts([0, 1], [3_600, 1], [7_200, 1], [10_800, 1]);
    expect(buildPlot(hourly, 0, 10_800, 0, 2).line.match(/M/g)).toHaveLength(1);
  });

  it('keeps one missed sample inside the line', () => {
    // 60 s cadence with one sample missing (120 s step) is not a gap.
    const p = buildPlot(pts([0, 1], [60, 1], [180, 1], [240, 1]), 0, 240, 0, 2);
    expect(p.line.match(/M/g)).toHaveLength(1);
  });

  it('draws a lone sample as a zero-length stroke (a dot)', () => {
    const p = buildPlot(pts([50, 1]), 0, 100, 0, 2);
    expect(p.line).toMatch(/^M\S+ \S+ l0 0$/);
    expect(p.area).toBe('');
  });

  it('copes with no points', () => {
    const p = buildPlot([], 0, 100, 0, 1);
    expect(p.xs).toEqual([]);
  });
});

describe('nearestIndex', () => {
  const p = pts([10, 0], [20, 0], [30, 0]);

  it('finds the closest sample', () => {
    expect(nearestIndex(p, 0)).toBe(0);
    expect(nearestIndex(p, 24)).toBe(1);
    expect(nearestIndex(p, 26)).toBe(2);
    expect(nearestIndex(p, 99)).toBe(2);
  });

  it('prefers the earlier sample on a tie and handles empty input', () => {
    expect(nearestIndex(p, 25)).toBe(1);
    expect(nearestIndex([], 5)).toBe(-1);
  });
});

describe('sampleRows', () => {
  const many = Array.from({ length: 100 }, (_, i) => ({ ts: i, value: i }));

  it('returns evenly spaced rows including both ends', () => {
    const rows = sampleRows(many, 5);
    expect(rows).toHaveLength(5);
    expect(rows[0].ts).toBe(0);
    expect(rows[4].ts).toBe(99);
    expect(new Set(rows.map((r) => r.ts)).size).toBe(5);
  });

  it('returns short input unchanged', () => {
    expect(sampleRows(many.slice(0, 3), 12)).toHaveLength(3);
  });
});

describe('niceScale', () => {
  it('spans data and threshold with three evenly spaced ticks', () => {
    const s = niceScale([52, 60], [85], false);
    expect(s.lo).toBeLessThanOrEqual(52);
    expect(s.hi).toBeGreaterThanOrEqual(85);
    expect(s.ticks[0]).toBe(s.hi);
    expect(s.ticks[2]).toBe(s.lo);
    expect(s.ticks[0] - s.ticks[1]).toBeCloseTo(s.ticks[1] - s.ticks[2]);
  });

  it('never goes negative for data that is not', () => {
    expect(niceScale([0.2, 0.4], [], false).lo).toBe(0);
    expect(niceScale([0, 0], [], false).lo).toBe(0);
  });

  it('allows a negative axis for negative data', () => {
    expect(niceScale([-5, 5], [], false).lo).toBeLessThanOrEqual(-5);
  });

  it('never goes above 100 for a percentage', () => {
    const s = niceScale([100, 100], [], true);
    expect(s.hi).toBe(100);
    expect(s.lo).toBeLessThan(100);
  });

  it('gives a flat series some room instead of a zero-height axis', () => {
    const s = niceScale([42, 42], [], false);
    expect(s.hi).toBeGreaterThan(s.lo);
    expect(s.lo).toBeLessThan(42);
    expect(s.hi).toBeGreaterThan(42);
  });

  it('reports enough decimals to print the ticks exactly', () => {
    const s = niceScale([99.2, 99.6], [], true);
    for (const t of s.ticks) expect(Number(t.toFixed(s.decimals))).toBeCloseTo(t, 6);
  });
});
