<script lang="ts">
  import type { Tone } from '../lib/model';
  import StatusMark from './StatusMark.svelte';

  let { meta, pill, pillTone }: { meta: string; pill: string; pillTone: Tone } = $props();
</script>

<header>
  <div class="id">
    <h1>healthd</h1>
    <span class="meta">{meta}</span>
  </div>
  <div class="pill">
    <StatusMark tone={pillTone} />
    <span>{pill}</span>
  </div>
</header>

<style>
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 8px 16px;
  }
  .id {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 4px 12px;
  }
  h1 {
    font-size: 20px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }
  .meta,
  .pill {
    font-size: 13px;
    color: var(--ink2);
  }
  .pill {
    display: flex;
    align-items: center;
    gap: 8px;
  }
</style>
