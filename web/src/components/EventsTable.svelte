<script lang="ts">
  import { formatTime } from '../lib/format';
  import { SEVERITY_LABEL } from '../lib/model';
  import type { DaemonEvent } from '../lib/types';
  import StatusMark from './StatusMark.svelte';

  let { events }: { events: DaemonEvent[] } = $props();

  /** Rows shown before "Show all". WHY 8: the latest few are what matter, and
   *  the section then stays about as tall as the cards above it. */
  const COLLAPSED_ROWS = 8;

  let showAll = $state(false);
  const shown = $derived(showAll ? events : events.slice(0, COLLAPSED_ROWS));
</script>

<section aria-labelledby="events-title">
  <div class="head">
    <h2 id="events-title">Events</h2>
    <span class="sub">Last 7 days · times in UTC</span>
  </div>

  {#if events.length === 0}
    <p class="empty">No events in the last 7 days.</p>
  {:else}
    <!-- Events have no id, and two can look identical, so the list is unkeyed. -->
    <ul>
      {#each shown as e}
        <li>
          <span class="time">{formatTime(e.ts)}</span>
          <span class="sev"><StatusMark tone={e.severity} /><b>{SEVERITY_LABEL[e.severity]}</b></span>
          <span class="src">{e.source}</span>
          <span class="msg">{e.msg}</span>
        </li>
      {/each}
    </ul>
    {#if events.length > COLLAPSED_ROWS}
      <button class="more" aria-expanded={showAll} onclick={() => (showAll = !showAll)}>
        {showAll ? 'Show fewer' : `Show all ${events.length}`}
      </button>
    {/if}
  {/if}
</section>

<style>
  section {
    padding: 16px 20px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
  }
  .head {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    justify-content: space-between;
    gap: 4px 16px;
    margin-bottom: 8px;
  }
  h2 {
    font-size: 16px;
    font-weight: 600;
  }
  .sub {
    font-size: 13px;
    color: var(--ink2);
  }
  .empty {
    margin: 12px 0 4px;
    color: var(--ink2);
  }
  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }
  li {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 2px 12px;
    padding: 10px 0;
    border-top: 1px solid var(--grid);
  }
  .time {
    width: 112px;
    flex: none;
    color: var(--ink2);
    font-variant-numeric: tabular-nums;
  }
  .sev {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 96px;
    flex: none;
  }
  .src {
    width: 64px;
    flex: none;
    color: var(--ink2);
  }
  .msg {
    flex: 1 1 260px;
    min-width: 0;
  }
  .more {
    min-height: 44px;
    margin-top: 4px;
    padding: 0 12px;
    background: none;
    border: 1px solid var(--line);
    border-radius: 8px;
    cursor: pointer;
  }
</style>
