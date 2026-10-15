<script lang="ts">
  import { formatDuration } from '../lib/format';
  import { SEVERITY_LABEL, type Tone } from '../lib/model';
  import type { ActiveAlert } from '../lib/types';
  import StatusMark from './StatusMark.svelte';

  let {
    tone,
    title,
    sub,
    alerts,
    nowSecs,
  }: { tone: Tone; title: string; sub: string; alerts: ActiveAlert[]; nowSecs: number } = $props();
</script>

<section class="banner" aria-label="Alert status">
  <div class="top">
    <StatusMark {tone} />
    <!-- Polite live region: a new alert is announced without stealing focus. -->
    <div class="text" aria-live="polite">
      <div class="title">{title}</div>
      <div class="sub">{sub}</div>
    </div>
  </div>
  {#if alerts.length > 0}
    <ul>
      {#each alerts as a (a.rule + '|' + a.source)}
        <li>
          <span class="sev">
            <StatusMark tone={a.severity} />
            <b>{SEVERITY_LABEL[a.severity]}</b>
          </span>
          <span class="msg">{a.msg}</span>
          <span class="meta">{a.source} · {formatDuration(nowSecs - a.since)} ago</span>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .banner {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px 20px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
  }
  .top {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .text {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .title {
    font-size: 18px;
    font-weight: 600;
  }
  .sub,
  .meta {
    font-size: 13px;
    color: var(--ink2);
  }
  ul {
    display: flex;
    flex-direction: column;
    margin: 0;
    padding: 0;
    list-style: none;
    border-top: 1px solid var(--grid);
  }
  li {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 12px;
    padding: 10px 0;
    border-bottom: 1px solid var(--grid);
  }
  li:last-child {
    border-bottom: 0;
    padding-bottom: 0;
  }
  .sev {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 96px;
    flex: none;
  }
  .msg {
    flex: 1 1 320px;
    min-width: 0;
  }
</style>
