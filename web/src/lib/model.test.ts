import { describe, expect, it } from 'vitest';
import {
  buildCards,
  isLiveOnly,
  metricLabel,
  ruleCovers,
  rulesFor,
  sourceTitle,
  toneOf,
  worstSeverity,
} from './model';
import type { ActiveAlert, Metric, RuleInfo } from './types';

const m = (source: string, metric: string, value: number): Metric => ({ source, metric, value, ts: 1 });

const alert = (source: string, metric: string, severity: ActiveAlert['severity']): ActiveAlert => ({
  rule: `${source}-${metric}`,
  severity,
  source,
  metric,
  value: 1,
  since: 0,
  msg: 'x',
});

const METRICS: Metric[] = [
  m('nvme0', 'temp_c', 41),
  m('nvme0', 'media_errors', 0),
  m('nvme0', 'percentage_used', 3),
  m('battery', 'charge_pct', 82),
  m('battery', 'discharging_pct', 82),
  m('battery', 'cycles', 140),
  m('ram', 'available_mb', 7408),
  m('cpu', 'load_pct', 12),
  m('cpu', 'temp_c', 55),
  m('fan0', 'rpm', 1200),
];

describe('buildCards', () => {
  const cards = buildCards(METRICS, []);

  it('orders known sources first, then unknown ones alphabetically', () => {
    expect(cards.map((c) => c.source)).toEqual(['cpu', 'ram', 'battery', 'nvme0', 'fan0']);
    expect(cards.map((c) => c.title)).toEqual(['CPU', 'Memory', 'Battery', 'NVMe · nvme0', 'fan0']);
  });

  it('picks the headline metric per kind and lists the rest in a fixed order', () => {
    const cpu = cards[0];
    expect(cpu.hero.metric).toBe('temp_c');
    expect(cpu.rows.map((r) => r.metric)).toEqual(['load_pct']);
    const nvme = cards[3];
    expect(nvme.rows.map((r) => r.metric)).toEqual(['percentage_used', 'media_errors']);
  });

  it('hides metrics that duplicate another', () => {
    const battery = cards[2];
    expect(battery.rows.map((r) => r.metric)).toEqual(['cycles']);
  });

  it('formats values with units', () => {
    expect(cards[0].hero.value).toBe('55°C');
    expect(cards[1].hero.value).toBe('7.2 GB');
  });

  it('marks memory as live only', () => {
    expect(cards[1].liveOnly).toBe(true);
    expect(cards[0].liveOnly).toBe(false);
  });

  it('falls back to the first metric for an unknown source', () => {
    expect(cards[4].hero.metric).toBe('rpm');
    expect(cards[4].hero.label).toBe('Rpm');
  });

  it('skips a source whose metrics are all hidden', () => {
    expect(buildCards([m('battery', 'discharging_pct', 50)], [])).toEqual([]);
  });

  it('uses the next hero when the first is missing', () => {
    const [cpu] = buildCards([m('cpu', 'load_pct', 5)], []);
    expect(cpu.hero.metric).toBe('load_pct');
    expect(cpu.rows).toEqual([]);
  });

  it('puts alerts on the card and on exactly the metric they watch', () => {
    const [cpu, , battery] = buildCards(METRICS, [
      alert('cpu', 'temp_c', 'crit'),
      alert('cpu', 'temp_c', 'warn'),
      alert('battery', 'cycles', 'warn'),
    ]);
    expect(cpu.tone).toBe('crit');
    expect(cpu.hero.alert).toBe('crit');
    expect(cpu.rows[0].alert).toBeNull();
    expect(battery.tone).toBe('warn');
    expect(battery.hero.alert).toBeNull();
    expect(battery.rows[0].alert).toBe('warn');
  });
});

describe('severity helpers', () => {
  it('finds the worst severity', () => {
    expect(worstSeverity([])).toBeNull();
    expect(worstSeverity(['info', 'crit', 'warn'])).toBe('crit');
    expect(worstSeverity(['info', 'warn'])).toBe('warn');
  });

  it('is ok when there are no alerts', () => {
    expect(toneOf([])).toBe('ok');
    expect(toneOf([alert('cpu', 'temp_c', 'warn')])).toBe('warn');
  });
});

describe('rules', () => {
  const rule = (source: string, metric: string): RuleInfo => ({
    name: `${source}-${metric}`,
    source,
    metric,
    op: '>',
    threshold: 1,
    severity: 'warn',
  });

  it('matches exact sources and trailing-star prefixes, like the daemon', () => {
    expect(ruleCovers('cpu', 'cpu')).toBe(true);
    expect(ruleCovers('cpu', 'cpu0')).toBe(false);
    expect(ruleCovers('nvme*', 'nvme0')).toBe(true);
    expect(ruleCovers('nvme*', 'cpu')).toBe(false);
  });

  it('selects the rules of one metric of one source', () => {
    const rules = [rule('nvme*', 'temp_c'), rule('cpu', 'temp_c'), rule('nvme*', 'media_errors')];
    expect(rulesFor(rules, 'nvme1', 'temp_c').map((r) => r.name)).toEqual(['nvme*-temp_c']);
    expect(rulesFor(rules, 'battery', 'temp_c')).toEqual([]);
  });
});

describe('labels', () => {
  it('has friendly names and a readable fallback', () => {
    expect(metricLabel('available_mb')).toBe('Available');
    expect(metricLabel('some_new_metric')).toBe('Some new metric');
  });

  it('knows card titles and live-only sources', () => {
    expect(sourceTitle('ram')).toBe('Memory');
    expect(sourceTitle('nvme2')).toBe('NVMe · nvme2');
    expect(sourceTitle('gpu')).toBe('gpu');
    expect(isLiveOnly('ram')).toBe(true);
    expect(isLiveOnly('cpu')).toBe(false);
  });
});
