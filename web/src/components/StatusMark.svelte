<script lang="ts">
  import type { Tone } from '../lib/model';

  // Status is never colour alone: each tone also has its own SHAPE (circle =
  // normal, triangle = warning, octagon = critical, ring = info, dashed ring =
  // stale) and is always printed next to a text label. This mark is decoration.
  let { tone }: { tone: Tone } = $props();
</script>

<span class="mark {tone}" aria-hidden="true"></span>

<style>
  .mark {
    display: inline-block;
    flex: none;
    width: 12px;
    height: 12px;
  }
  .ok {
    background: #0ca30c;
    border-radius: 50%;
  }
  .warn {
    background: #fab219;
    clip-path: polygon(50% 0, 100% 100%, 0 100%);
  }
  .crit {
    background: #d03b3b;
    clip-path: polygon(30% 0, 70% 0, 100% 30%, 100% 70%, 70% 100%, 30% 100%, 0 70%, 0 30%);
  }
  .info {
    border: 2px solid var(--quiet);
    border-radius: 50%;
  }
  .stale {
    border: 2px dashed var(--quiet);
    border-radius: 50%;
  }
</style>
