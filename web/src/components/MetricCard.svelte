<script lang="ts">
  import { buildPlot, VIEW_H, VIEW_W } from '../lib/chart';
  import { SEVERITY_LABEL, TONE_LABEL, type CardModel, type RowModel } from '../lib/model';
  import type { Selection } from '../lib/store.svelte';
  import type { Point } from '../lib/types';
  import StatusMark from './StatusMark.svelte';

  let {
    card,
    selected,
    offline,
    spark,
    sparkSince,
    sparkUntil,
    onselect,
  }: {
    card: CardModel;
    selected: Selection | null;
    /** No live data: the card is dimmed and marked stale. */
    offline: boolean;
    /** Recent history of the headline metric, for the sparkline. */
    spark: Point[] | undefined;
    sparkSince: number;
    sparkUntil: number;
    onselect: (source: string, metric: string) => void;
  } = $props();

  const tone = $derived(offline ? 'stale' : card.tone);

  const isSelected = (r: RowModel) => selected?.source === r.source && selected.metric === r.metric;

  /**
   * Sparkline path for the headline metric. It only shows the trend, so the
   * value range is simply the data's own range with a little padding (no axis,
   * no labels); a flat series sits in the middle.
   */
  const plot = $derived.by(() => {
    if (!spark || spark.length < 2) return null;
    const values = spark.map((p) => p.value);
    const lo = Math.min(...values);
    const hi = Math.max(...values);
    const pad = Math.max((hi - lo) * 0.15, 1);
    return buildPlot(spark, sparkSince, sparkUntil, lo - pad, hi + pad);
  });
</script>

{#snippet alertMark(r: RowModel)}
  {#if r.alert}
    <StatusMark tone={r.alert} />
    <span class="sr">{SEVERITY_LABEL[r.alert]}:</span>
  {/if}
{/snippet}

<article class="card" class:stale={offline} aria-label={card.title}>
  <header>
    <h2>{card.title}</h2>
    <span class="tone"><StatusMark {tone} />{TONE_LABEL[tone]}</span>
  </header>

  <button class="hero" class:picked={isSelected(card.hero)} aria-pressed={isSelected(card.hero)} onclick={() => onselect(card.hero.source, card.hero.metric)}>
    <span class="label">{card.hero.label}</span>
    <span class="value">{@render alertMark(card.hero)}{card.hero.value}</span>
  </button>

  {#if card.liveOnly}
    <p class="note">Live only · not stored</p>
  {:else if plot}
    <svg class="spark" viewBox="0 0 {VIEW_W} {VIEW_H}" preserveAspectRatio="none" role="img" aria-label="Last 6 hours of {card.hero.label.toLowerCase()}">
      <path class="area" d={plot.area} />
      <path class="line" d={plot.line} />
    </svg>
  {:else}
    <div class="spark empty"></div>
  {/if}

  {#if card.rows.length > 0}
    <ul>
      {#each card.rows as r (r.metric)}
        <li>
          <button class="row" class:picked={isSelected(r)} aria-pressed={isSelected(r)} onclick={() => onselect(r.source, r.metric)}>
            <span class="label">{r.label}</span>
            <span class="rv">{@render alertMark(r)}{r.value}</span>
          </button>
        </li>
      {/each}
    </ul>
  {/if}
</article>

<style>
  .card {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 16px 12px 8px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
  }
  /* Dimmed, not hidden: the last reading may still be useful, but must not pass for live. */
  .stale {
    opacity: 0.55;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 0 8px;
  }
  h2 {
    font-size: 14px;
    font-weight: 600;
  }
  .tone {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--ink2);
  }

  /* Both kinds of button are 44 px tall: comfortable to hit on a phone. */
  button.hero,
  button.row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    min-height: 44px;
    padding: 6px 8px;
    background: none;
    border: 0;
    border-radius: 8px;
    text-align: left;
    cursor: pointer;
  }
  button.hero {
    align-items: baseline;
  }
  button.hero:hover,
  button.row:hover {
    background: var(--wash);
  }
  /* The bar is the non-colour cue for the metric shown in the chart. */
  button.picked {
    background: var(--wash);
    box-shadow: inset 3px 0 0 var(--series);
    /* Nearly square on the left so the bar reads as a bar, not a bracket. */
    border-radius: 2px 8px 8px 2px;
  }
  .label {
    color: var(--ink2);
  }
  .hero .value {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 28px;
    font-weight: 600;
    letter-spacing: -0.01em;
    font-variant-numeric: tabular-nums;
  }
  .rv {
    display: flex;
    align-items: center;
    gap: 6px;
    font-variant-numeric: tabular-nums;
  }

  .spark {
    display: block;
    width: calc(100% - 16px);
    height: 36px;
    margin: 0 8px;
  }
  .empty {
    border-bottom: 1px dashed var(--axis);
    height: 18px;
  }
  .line,
  .area {
    stroke: none;
  }
  .area {
    fill: var(--area);
  }
  .line {
    fill: none;
    stroke: var(--series);
    stroke-width: 1.5;
    stroke-linecap: round;
    stroke-linejoin: round;
    vector-effect: non-scaling-stroke;
  }
  .note {
    margin: 0;
    padding: 0 8px;
    font-size: 12px;
    color: var(--muted);
  }

  ul {
    margin: 0;
    padding: 4px 0 0;
    list-style: none;
    border-top: 1px solid var(--grid);
  }
</style>
