// Typed client for the daemon's /api/* routes. They return exactly what the
// Unix-socket API returns: `{ok, data}` or `{ok:false, error}`.

import type { EventsData, HistoryData, StatusData } from './types';

/**
 * Give up on a request after this long.
 *
 * WHY 4 s: the daemon answers from RAM or a tiny SQLite file in milliseconds,
 * and polls are 5 s apart, so a slower reply means the daemon is wedged and the
 * dashboard should say so instead of stacking up requests.
 */
const TIMEOUT_MS = 4_000;

interface Envelope<T> {
  ok: boolean;
  data?: T;
  error?: string;
}

type Params = Record<string, string | number | undefined>;

/** GET `/api/<name>` and unwrap the envelope; throws `Error` with a readable message. */
async function get<T>(name: string, params: Params = {}): Promise<T> {
  const qs = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined) qs.set(k, String(v));
  }
  const query = qs.toString();
  const res = await fetch(`/api/${name}${query ? `?${query}` : ''}`, {
    cache: 'no-store',
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  let body: Envelope<T>;
  try {
    body = (await res.json()) as Envelope<T>;
  } catch {
    throw new Error(`unexpected reply (HTTP ${res.status})`);
  }
  if (!body.ok || body.data === undefined) {
    throw new Error(body.error ?? `HTTP ${res.status}`);
  }
  return body.data;
}

export const api = {
  /** Latest value of every metric, active alerts and the configured rules. */
  status: () => get<StatusData>('status'),

  /** Stored samples of one metric since a Unix time; the newest `limit` are kept. */
  history: (source: string, metric: string, since: number, limit: number) =>
    get<HistoryData>('history', { source, metric, since, limit }),

  /** Recent events, oldest first. */
  events: (since: number, limit: number) => get<EventsData>('events', { since, limit }),
};
