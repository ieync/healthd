<script lang="ts">
  import { buildPlot, nearestIndex, niceScale, sampleRows, VIEW_H, VIEW_W } from '../lib/chart';
  import { formatTick, formatTime, formatWithUnit, unitFor } from '../lib/format';
  import { isLiveOnly, metricLabel, SEVERITY_LABEL, sourceTitle } from '../lib/model';
  import { WINDOWS, type Dashboard } from '../lib/store.svelte';

  let { dash }: { dash: Dashboard } = $props();

  /** Rows in the table view. WHY 12: enough to read a trend, short enough to scan. */
  const TABLE_ROWS = 12;

  /** How a rule's operator reads next to its threshold. */
  const OP_WORDS = { '>': 'above', '>=': 'at or above', '<': 'below', '<=': 'at or below' } as const;

  /** Index of the point under the pointer, or null. */
  let hover = $state<number | null>(null);

  const sel = $derived(dash.selected);
  const win = $derived(WINDOWS[dash.windowIdx]);
  const liveOnly = $derived(sel !== null && isLiveOnly(sel.source));
  /** `until` is 0 until the reply for the current selection and window has arrived. */
  const loaded = $derived(dash.chart.until > 0);
  const points = $derived(dash.chart.points);
  const heading = $derived(sel ? `${sourceTitle(sel.source)} · ${metricLabel(sel.metric)}` : 'History');
  const metric = $derived(sel?.metric ?? '');

  /** Everything drawn on the chart; null when there is nothing to draw. */
  const view = $derived.by(() => {
    if (points.length === 0) return null;
    const values = points.map((p) => p.value);
    const thresholds = dash.thresholds;
    const scale = niceScale(
      values,
      thresholds.map((t) => t.threshold),
      unitFor(metric) === '%',
    );
    const plot = buildPlot(points, dash.chart.since, dash.chart.until, scale.lo, scale.hi);
    const lines = thresholds.map((t) => ({
      key: t.name,
      // Percent from the top of the plot, like the points.
      y: Math.min(100, Math.max(0, 100 - ((t.threshold - scale.lo) / (scale.hi - scale.lo)) * 100)),
      label: `${SEVERITY_LABEL[t.severity]}: ${OP_WORDS[t.op]} ${formatWithUnit(metric, t.threshold)}`,
    }));
    const sum = values.reduce((a, b) => a + b, 0);
    const stats = [
      ['Min', Math.min(...values)],
      ['Avg', sum / values.length],
      ['Max', Math.max(...values)],
      ['Latest', values[values.length - 1]],
    ] as const;
    return { scale, plot, lines, stats };
  });

  const hovered = $derived(
    view && hover !== null && hover < points.length
      ? { point: points[hover], x: view.plot.xs[hover], y: view.plot.ys[hover] }
      : null,
  );

  const summary = $derived(
    view
      ? `${heading}, last ${win.label}. ` + view.stats.map(([name, v]) => `${name} ${formatWithUnit(metric, v)}`).join(', ')
      : heading,
  );

  /** Follow the pointer along the time axis and snap to the nearest sample. */
  function onMove(e: PointerEvent): void {
    const box = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (box.width === 0) return;
    const f = Math.min(1, Math.max(0, (e.clientX - box.left) / box.width));
    const { since, until } = dash.chart;
    const i = nearestIndex(points, since + f * (until - since));
    hover = i >= 0 ? i : null;
  }

  function pickWindow(i: number): void {
    hover = null;
    dash.setWindow(i);
  }
</script>

<section class="chart" aria-labelledby="chart-title">
  <div class="head">
    <div>
      <h2 id="chart-title">{heading}</h2>
      <p class="sub">Last {win.label}{win.label === '7d' ? ' · older data is hourly averages' : ''}</p>
    </div>
    <div class="controls">
      <div class="seg" role="group" aria-label="Time window">
        {#each WINDOWS as w, i (w.label)}
          <button aria-pressed={dash.windowIdx === i} disabled={liveOnly} onclick={() => pickWindow(i)}>{w.label}</button>
        {/each}
      </div>
      <button class="toggle" aria-pressed={dash.tableView} disabled={liveOnly} onclick={() => (dash.tableView = !dash.tableView)}>Table</button>
    </div>
  </div>

  {#if !sel}
    <p class="state">Waiting for data…</p>
  {:else if liveOnly}
    <p class="state">
      <span><b>Live only.</b> The daemon keeps {sourceTitle(sel.source).toLowerCase()} readings in RAM and never writes them to disk, so there is no history to chart. The card above shows the current value.</span>
    </p>
  {:else if !loaded && dash.historyError}
    <p class="state">Could not load history ({dash.historyError}). It will be retried.</p>
  {:else if !loaded}
    <p class="state">Loading…</p>
  {:else if !view}
    <p class="state">No samples in the last {win.label}. The daemon stores one a minute once it has been running.</p>
  {:else if dash.tableView}
    {@const rows = sampleRows(points, TABLE_ROWS)}
    <div class="tablewrap">
      <table>
        <caption class="sr">{summary}</caption>
        <thead>
          <tr><th scope="col">Time (UTC)</th><th scope="col" class="num">{metricLabel(metric)}</th></tr>
        </thead>
        <tbody>
          {#each rows as p (p.ts)}
            <tr><td>{formatTime(p.ts)}</td><td class="num">{formatWithUnit(metric, p.value)}</td></tr>
          {/each}
        </tbody>
      </table>
    </div>
    <p class="sub">{rows.length} of {points.length} samples, evenly spaced.</p>
  {:else}
    <div class="stage">
      <div class="ylabels" aria-hidden="true">
        <span class="top">{formatTick(metric, view.scale.ticks[0], view.scale.decimals)}</span>
        <span class="mid">{formatTick(metric, view.scale.ticks[1], view.scale.decimals)}</span>
        <span class="bottom">{formatTick(metric, view.scale.ticks[2], view.scale.decimals)}</span>
      </div>

      <!-- The pointer is a mouse/touch extra: the same numbers are in the Table view. -->
      <div class="plot" role="presentation" onpointermove={onMove} onpointerleave={() => (hover = null)}>
        <div class="g top"></div>
        <div class="g mid"></div>
        <div class="g bottom"></div>

        <svg viewBox="0 0 {VIEW_W} {VIEW_H}" preserveAspectRatio="none" role="img" aria-label={summary}>
          <path class="area" d={view.plot.area} />
          <path class="line" d={view.plot.line} />
        </svg>

        {#each view.lines as t (t.key)}
          <div class="thr" class:low={t.y < 14} style:top="{t.y}%"><span>{t.label}</span></div>
        {/each}

        {#if view.plot.xs.length > 0}
          <span class="dot" style:left="{view.plot.xs[view.plot.xs.length - 1]}%" style:top="{view.plot.ys[view.plot.ys.length - 1]}%"></span>
        {/if}

        {#if hovered}
          <div class="cross" style:left="{hovered.x}%"></div>
          <span class="dot on" style:left="{hovered.x}%" style:top="{hovered.y}%"></span>
          <div class="tip" class:below={hovered.y < 50} style:left="{Math.min(88, Math.max(12, hovered.x))}%">
            <b>{formatWithUnit(metric, hovered.point.value)}</b>
            <span>{formatTime(hovered.point.ts)} UTC</span>
          </div>
        {/if}
      </div>

      <div class="xlabels" aria-hidden="true">
        <span>-{win.label}</span>
        <span>{win.mid}</span>
        <span>now</span>
      </div>
    </div>

    <dl class="stats">
      {#each view.stats as [name, v] (name)}
        <div><dt>{name}</dt><dd>{formatWithUnit(metric, v)}</dd></div>
      {/each}
    </dl>
    {#if dash.historyError}
      <p class="sub">Could not refresh ({dash.historyError}); showing the last result.</p>
    {/if}
  {/if}
</section>

<style>
  .chart {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px 20px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-start;
    justify-content: space-between;
    gap: 8px 16px;
  }
  h2 {
    font-size: 16px;
    font-weight: 600;
  }
  .sub {
    margin: 2px 0 0;
    font-size: 13px;
    color: var(--ink2);
  }
  .controls {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .seg {
    display: flex;
    border: 1px solid var(--line);
    border-radius: 8px;
    overflow: hidden;
  }
  .seg button,
  .toggle {
    min-width: 44px;
    min-height: 44px;
    padding: 0 12px;
    background: none;
    border: 0;
    cursor: pointer;
  }
  .toggle {
    border: 1px solid var(--line);
    border-radius: 8px;
  }
  .seg button + button {
    border-left: 1px solid var(--line);
  }
  /* Filled background = the selected option; the pressed state is also exposed to assistive tech. */
  button[aria-pressed='true'] {
    background: var(--wash);
    font-weight: 600;
  }
  button:disabled {
    color: var(--muted);
    cursor: default;
  }

  .state {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 220px;
    margin: 0;
    padding: 0 16px;
    text-align: center;
    color: var(--ink2);
  }

  .stage {
    display: grid;
    grid-template-columns: auto 1fr;
    column-gap: 8px;
    padding: 8px 0 0;
  }
  .ylabels {
    position: relative;
    height: 220px;
    min-width: 40px;
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }
  .ylabels span {
    position: absolute;
    right: 0;
    transform: translateY(-50%);
    white-space: nowrap;
  }
  .top {
    top: 0;
  }
  .mid {
    top: 50%;
  }
  .bottom {
    top: 100%;
  }
  .plot {
    position: relative;
    height: 220px;
    touch-action: pan-y;
  }
  .g {
    position: absolute;
    left: 0;
    right: 0;
    border-top: 1px solid var(--grid);
  }
  .g.bottom {
    border-top-color: var(--axis);
  }
  svg {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    overflow: visible;
  }
  .area {
    fill: var(--area);
    stroke: none;
  }
  .line {
    fill: none;
    stroke: var(--series);
    stroke-width: 2;
    stroke-linecap: round;
    stroke-linejoin: round;
    vector-effect: non-scaling-stroke;
  }
  .thr {
    position: absolute;
    left: 0;
    right: 0;
    border-top: 1.5px dashed var(--ink2);
    pointer-events: none;
  }
  .thr span {
    position: absolute;
    right: 4px;
    bottom: 2px;
    padding: 0 4px;
    font-size: 11px;
    color: var(--ink2);
    background: var(--surface);
    border-radius: 4px;
  }
  .thr.low span {
    bottom: auto;
    top: 2px;
  }
  .dot {
    position: absolute;
    width: 9px;
    height: 9px;
    background: var(--series);
    border: 2px solid var(--surface);
    border-radius: 50%;
    transform: translate(-50%, -50%);
    pointer-events: none;
  }
  .dot.on {
    width: 11px;
    height: 11px;
  }
  .cross {
    position: absolute;
    top: 0;
    bottom: 0;
    border-left: 1px solid var(--axis);
    pointer-events: none;
  }
  .tip {
    position: absolute;
    top: 6px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px 10px;
    font-size: 12px;
    color: var(--ink2);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 8px;
    box-shadow: 0 2px 8px var(--shadow);
    transform: translateX(-50%);
    white-space: nowrap;
    pointer-events: none;
  }
  .tip.below {
    top: auto;
    bottom: 6px;
  }
  .tip b {
    font-size: 14px;
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  .xlabels {
    grid-column: 2;
    display: flex;
    justify-content: space-between;
    padding-top: 6px;
    font-size: 12px;
    color: var(--muted);
  }

  .stats {
    display: flex;
    flex-wrap: wrap;
    gap: 8px 32px;
    margin: 0;
    padding-top: 12px;
    border-top: 1px solid var(--grid);
  }
  .stats div {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  dt {
    font-size: 12px;
    color: var(--muted);
  }
  dd {
    margin: 0;
    font-size: 16px;
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }

  .tablewrap {
    overflow-x: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-variant-numeric: tabular-nums;
  }
  th,
  td {
    padding: 10px 8px;
    text-align: left;
    border-bottom: 1px solid var(--grid);
  }
  th {
    font-size: 12px;
    font-weight: 500;
    color: var(--muted);
  }
  .num {
    text-align: right;
  }
</style>
