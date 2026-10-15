import { describe, expect, it } from 'vitest';
import { formatDuration, formatTick, formatTime, formatValue, formatWithUnit, unitFor } from './format';

describe('formatValue', () => {
  it('uses one decimal below 100 and none from 100 up', () => {
    expect(formatValue(61.5)).toBe('61.5');
    expect(formatValue(82)).toBe('82');
    expect(formatValue(7408.3)).toBe('7408');
  });

  it('never prints a negative zero or NaN', () => {
    expect(formatValue(-0.04)).toBe('0.0');
    expect(formatValue(-0)).toBe('0');
    expect(formatValue(Number.NaN)).toBe('n/a');
  });
});

describe('unitFor', () => {
  it('follows the pollers naming convention', () => {
    expect(unitFor('temp_c')).toBe('°C');
    expect(unitFor('load_pct')).toBe('%');
    expect(unitFor('available_mb')).toBe(' MB');
    expect(unitFor('percentage_used')).toBe('%');
    expect(unitFor('avail_spare')).toBe('%');
    expect(unitFor('hours_held_full')).toBe(' h');
    expect(unitFor('cycles')).toBe('');
  });
});

describe('formatWithUnit', () => {
  it('appends the unit', () => {
    expect(formatWithUnit('temp_c', 61.5)).toBe('61.5°C');
    expect(formatWithUnit('charge_pct', 82)).toBe('82%');
  });

  it('switches megabytes to gigabytes from 1000 up', () => {
    expect(formatWithUnit('available_mb', 999)).toBe('999 MB');
    expect(formatWithUnit('available_mb', 7408)).toBe('7.2 GB');
  });
});

describe('formatTick', () => {
  it('keeps a fixed number of decimals', () => {
    expect(formatTick('temp_c', 40, 0)).toBe('40°C');
    expect(formatTick('load_pct', 12.5, 1)).toBe('12.5%');
    expect(formatTick('temp_c', -0.001, 1)).toBe('0.0°C');
  });

  it('uses gigabytes for large memory ticks', () => {
    expect(formatTick('available_mb', 4096, 0)).toBe('4.0 GB');
  });
});

describe('formatDuration', () => {
  it('picks the unit by size', () => {
    expect(formatDuration(42)).toBe('42s');
    expect(formatDuration(420)).toBe('7m');
    expect(formatDuration(3 * 3600 + 5 * 60)).toBe('3h 05m');
    expect(formatDuration(2 * 86_400 + 4 * 3600)).toBe('2d 4h');
  });

  it('treats negative input (clock skew) as zero', () => {
    expect(formatDuration(-5)).toBe('0s');
  });
});

describe('formatTime', () => {
  it('prints UTC regardless of the browser time zone', () => {
    expect(formatTime(Date.UTC(2026, 9, 14, 2, 54) / 1000)).toBe('Oct 14, 02:54');
    expect(formatTime(Date.UTC(2026, 0, 3, 23, 5) / 1000)).toBe('Jan 3, 23:05');
  });
});
