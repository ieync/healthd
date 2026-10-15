// All dashboard state and polling in one place, so components stay declarative.
//
// Everything is pulled from the daemon over /api/*; nothing is pushed. Polling
// pauses while the tab is hidden (no point asking the daemon for a screen
// nobody sees) and catches up immediately when the tab comes back.

import { api } from './api';
import { buildCards, isLiveOnly, rulesFor, type CardModel } from './model';
import type { DaemonEvent, Point, RuleInfo, StatusData } from './types';

/** Status poll period. WHY 5 s: the daemon samples CPU and RAM every 5 s, so
 *  asking faster than that only repeats the same numbers. */
export const STATUS_EVERY_MS = 5_000;

/** Events poll period. WHY 15 s: events are rare, and a changed set of active
 *  alerts triggers an immediate refresh anyway (see `refreshStatus`). */
const EVENTS_EVERY_MS = 15_000;

/** Chart refresh period. WHY 30 s: the daemon stores one sample a minute, so a
 *  faster refresh would only re-draw identical data. */
const HISTORY_EVERY_MS = 30_000;

/** Sparkline refresh period. WHY 60 s: same reason, and sparklines are minor. */
const SPARKS_EVERY_MS = 60_000;

/** Sparklines show the last six hours: a trend, not detail. */
const SPARK_WINDOW_SECS = 6 * 3_600;

/** WHY 500: six hours of one-minute samples is 360 points. */
const SPARK_LIMIT = 500;

/** Most points requested for the chart. WHY 4000: the same limit as the
 *  terminal UI. A 7-day window holds 48 h of minute rows (2 880) plus 5 days of
 *  hourly rows (120), and the daemon keeps the *newest* N, so the limit must
 *  exceed that or the chart's left edge would be cut off. */
const HISTORY_LIMIT = 4_000;

/** The events list covers a week. WHY 7 d / 100: recent enough to matter,
 *  small enough for one reply. */
const EVENT_LOOKBACK_SECS = 7 * 86_400;
const EVENTS_FETCH = 100;

/** Chart windows. `mid` labels the middle of the x axis. */
export const WINDOWS = [
  { label: '1h', secs: 3_600, mid: '-30m' },
  { label: '6h', secs: 21_600, mid: '-3h' },
  { label: '24h', secs: 86_400, mid: '-12h' },
  { label: '7d', secs: 604_800, mid: '-3.5d' },
] as const;

/** Default window: index 1 (6 h), long enough to show a trend, short enough that
 *  full-resolution minute data fills the chart. */
const DEFAULT_WINDOW = 1;

export interface Selection {
  source: string;
  metric: string;
}

/** Key of a (source, metric) pair, for lookups. */
export const metricKey = (source: string, metric: string) => `${source}|${metric}`;

/** A one-line reason for a failed request. */
function messageOf(e: unknown): string {
  if (e instanceof DOMException && e.name === 'TimeoutError') return 'timed out';
  return e instanceof Error ? e.message : String(e);
}

export class Dashboard {
  /** Last good `status` reply. Kept while the daemon is unreachable. */
  status = $state.raw<StatusData | null>(null);
  /** Why the last status poll failed, or null while the daemon answers. */
  error = $state<string | null>(null);
  /** Time of the last good status reply (ms since epoch); 0 = never. */
  lastOk = $state(0);
  /** Ticking clock for "updated 3 s ago" and alert ages. */
  now = $state(Date.now());

  /** Events, newest first. */
  events = $state.raw<DaemonEvent[]>([]);

  selected = $state.raw<Selection | null>(null);
  windowIdx = $state(DEFAULT_WINDOW);
  tableView = $state(false);

  #history = $state.raw<{ key: string; points: Point[]; since: number; until: number } | null>(null);
  historyLoading = $state(false);
  historyError = $state<string | null>(null);
  sparks = $state.raw<Record<string, Point[]>>({});
  sparkSince = $state(0);
  sparkUntil = $state(0);

  cards: CardModel[] = $derived(
    this.status ? buildCards(this.status.metrics, this.status.active_alerts) : [],
  );
  rules: RuleInfo[] = $derived(this.status?.rules ?? []);

  /** Chart data for the current selection and window; empty while it loads. */
  chart = $derived.by(() => {
    const h = this.#history;
    const empty = { points: [] as Point[], since: 0, until: 0 };
    return h && h.key === this.#currentKey() ? h : empty;
  });

  /** Thresholds of the rules watching the selected metric. */
  thresholds = $derived(
    this.selected ? rulesFor(this.rules, this.selected.source, this.selected.metric) : [],
  );

  #timeouts = new Map<string, number>();
  #clock = 0;
  #stopped = true;
  #seq = 0;
  #alertKey = '';
  #sparkKeys = '';

  #currentKey(): string {
    const s = this.selected;
    return s ? `${s.source}|${s.metric}|${this.windowIdx}` : '';
  }

  /** Begin polling. Call once from `onMount`; pair with [`stop`](#stop). */
  start(): void {
    this.#stopped = false;
    this.#loop('status', () => this.refreshStatus(), STATUS_EVERY_MS);
    this.#loop('events', () => this.refreshEvents(), EVENTS_EVERY_MS);
    this.#loop('history', () => this.refreshHistory(), HISTORY_EVERY_MS);
    this.#loop('sparks', () => this.refreshSparks(), SPARKS_EVERY_MS);
    this.#clock = window.setInterval(() => {
      if (!document.hidden) this.now = Date.now();
    }, 1_000);
    document.addEventListener('visibilitychange', this.#onVisible);
  }

  /** Stop all timers. */
  stop(): void {
    this.#stopped = true;
    for (const id of this.#timeouts.values()) window.clearTimeout(id);
    this.#timeouts.clear();
    window.clearInterval(this.#clock);
    document.removeEventListener('visibilitychange', this.#onVisible);
  }

  /** Refresh everything now when the tab becomes visible again. */
  #onVisible = (): void => {
    if (document.hidden) return;
    this.now = Date.now();
    void this.refreshStatus();
    void this.refreshEvents();
    void this.refreshHistory();
    void this.refreshSparks();
  };

  /** Run `fn` now and then every `ms` (after the previous run finished, so a
   *  slow daemon never gets overlapping requests); skipped while hidden. */
  #loop(name: string, fn: () => Promise<void>, ms: number): void {
    const tick = async () => {
      if (this.#stopped) return;
      if (!document.hidden) await fn();
      if (!this.#stopped) this.#timeouts.set(name, window.setTimeout(tick, ms));
    };
    void tick();
  }

  async refreshStatus(): Promise<void> {
    try {
      const s = await api.status();
      this.status = s;
      this.error = null;
      this.lastOk = Date.now();
      this.now = this.lastOk;

      if (this.#reselect()) void this.refreshHistory();
      // WHY: a new or cleared alert is exactly when the events list changes.
      const alertKey = s.active_alerts.map((a) => `${a.rule}|${a.source}|${a.since}`).join(',');
      if (alertKey !== this.#alertKey) {
        this.#alertKey = alertKey;
        void this.refreshEvents();
      }
      // The first reply (or a new drive appearing) needs sparklines right away.
      const sparkKeys = this.cards
        .filter((c) => !c.liveOnly)
        .map((c) => metricKey(c.hero.source, c.hero.metric))
        .join(',');
      if (sparkKeys !== this.#sparkKeys) {
        this.#sparkKeys = sparkKeys;
        void this.refreshSparks();
      }
    } catch (e) {
      this.error = messageOf(e);
    }
  }

  async refreshEvents(): Promise<void> {
    try {
      const until = Math.floor(Date.now() / 1000);
      const data = await api.events(until - EVENT_LOOKBACK_SECS, EVENTS_FETCH);
      this.events = [...data.events].reverse();
    } catch {
      // Keep the old list; the status poll is what reports connectivity.
    }
  }

  async refreshHistory(): Promise<void> {
    const sel = this.selected;
    if (!sel || isLiveOnly(sel.source)) {
      this.#history = null;
      this.historyLoading = false;
      this.historyError = null;
      return;
    }
    const key = this.#currentKey();
    const seq = ++this.#seq;
    this.historyLoading = true;
    const until = Math.floor(Date.now() / 1000);
    const since = until - WINDOWS[this.windowIdx].secs;
    try {
      const data = await api.history(sel.source, sel.metric, since, HISTORY_LIMIT);
      if (seq !== this.#seq) return; // a newer request superseded this one
      this.#history = { key, points: data.points, since, until };
      this.historyError = null;
    } catch (e) {
      if (seq === this.#seq) this.historyError = messageOf(e);
    } finally {
      if (seq === this.#seq) this.historyLoading = false;
    }
  }

  async refreshSparks(): Promise<void> {
    const heroes = this.cards.filter((c) => !c.liveOnly).map((c) => c.hero);
    if (heroes.length === 0) return;
    const until = Math.floor(Date.now() / 1000);
    const since = until - SPARK_WINDOW_SECS;
    const results = await Promise.allSettled(
      heroes.map((h) => api.history(h.source, h.metric, since, SPARK_LIMIT)),
    );
    // A failed request keeps that card's previous sparkline.
    const next = { ...this.sparks };
    results.forEach((r, i) => {
      if (r.status === 'fulfilled') next[metricKey(heroes[i].source, heroes[i].metric)] = r.value.points;
    });
    this.sparks = next;
    this.sparkSince = since;
    this.sparkUntil = until;
  }

  /** Make sure the selection still exists; returns true if it had to change. */
  #reselect(): boolean {
    const sel = this.selected;
    const exists =
      sel !== null &&
      this.cards.some(
        (c) =>
          c.source === sel.source &&
          (c.hero.metric === sel.metric || c.rows.some((r) => r.metric === sel.metric)),
      );
    if (exists) return false;
    const first = this.cards[0]?.hero;
    this.selected = first ? { source: first.source, metric: first.metric } : null;
    return true;
  }

  /** Show another metric in the chart. */
  select(source: string, metric: string): void {
    this.selected = { source, metric };
    void this.refreshHistory();
  }

  /** Choose a chart window by index into [`WINDOWS`](#WINDOWS). */
  setWindow(i: number): void {
    this.windowIdx = i;
    void this.refreshHistory();
  }
}
