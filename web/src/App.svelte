<script lang="ts">
  import { onMount } from 'svelte';
  import AlertBanner from './components/AlertBanner.svelte';
  import EventsTable from './components/EventsTable.svelte';
  import Header from './components/Header.svelte';
  import HistoryChart from './components/HistoryChart.svelte';
  import MetricCard from './components/MetricCard.svelte';
  import { formatDuration } from './lib/format';
  import { SEVERITY_LABEL, toneOf, worstSeverity, type Tone } from './lib/model';
  import { Dashboard, metricKey, STATUS_EVERY_MS } from './lib/store.svelte';

  const dash = new Dashboard();

  onMount(() => {
    dash.start();
    return () => dash.stop();
  });

  const status = $derived(dash.status);
  /** The last status poll failed: whatever is on screen is no longer live. */
  const offline = $derived(dash.error !== null);
  const alerts = $derived(status?.active_alerts ?? []);
  const nowSecs = $derived(Math.floor(dash.now / 1000));
  const sinceOk = $derived(Math.max(0, nowSecs - Math.floor(dash.lastOk / 1000)));
  const retry = `retrying every ${STATUS_EVERY_MS / 1000} s`;

  /** Page-level tone: stale whenever there is no live data, else the worst alert. */
  const tone = $derived<Tone>(status === null || offline ? 'stale' : toneOf(alerts));

  const banner = $derived.by(() => {
    if (status === null && !offline) {
      return { title: 'Connecting…', sub: 'Waiting for the first reply from the daemon.' };
    }
    if (offline) {
      return {
        title: 'Daemon not responding',
        sub: status
          ? `Showing the last reading, ${formatDuration(sinceOk)} old (${dash.error}). Reconnects automatically.`
          : `Cannot reach healthd (${dash.error}). Reconnects automatically.`,
      };
    }
    const worst = worstSeverity(alerts.map((a) => a.severity));
    if (worst !== null) {
      const n = alerts.length;
      return {
        title: `${SEVERITY_LABEL[worst]}: ${n} active alert${n === 1 ? '' : 's'}`,
        sub: 'An alert clears by itself once the reading is back in range.',
      };
    }
    const rules = status?.rules?.length ?? 0;
    return {
      title: 'All systems normal',
      sub: rules > 0 ? `${dash.cards.length} sources watched by ${rules} alert rules.` : `${dash.cards.length} sources reporting.`,
    };
  });

  const meta = $derived(status ? `${status.version} · up ${formatDuration(status.uptime_secs + sinceOk)}` : '');
  const pill = $derived(
    status === null && !offline
      ? 'Connecting…'
      : offline
        ? `Disconnected · ${retry}`
        : `Live · updated ${formatDuration(sinceOk)} ago`,
  );

  // Built as one string: Svelte trims the whitespace around an {#if} block, which
  // had glued "0.1.0 ·" to "served".
  const footer = $derived(
    (status ? `healthd ${status.version} · ` : '') + `served by the daemon on ${location.host} · same API as healthctl`,
  );

  // The tab title shows the alert count, so a pinned tab is a status light.
  $effect(() => {
    document.title = alerts.length > 0 ? `(${alerts.length}) healthd` : 'healthd';
  });
</script>

<main>
  <Header {meta} {pill} pillTone={status === null || offline ? 'stale' : 'ok'} />
  <AlertBanner {tone} title={banner.title} sub={banner.sub} {alerts} {nowSecs} />

  {#if dash.cards.length > 0}
    <section class="cards" aria-label="Sensors">
      {#each dash.cards as card (card.source)}
        <MetricCard
          {card}
          {offline}
          selected={dash.selected}
          spark={dash.sparks[metricKey(card.hero.source, card.hero.metric)]}
          sparkSince={dash.sparkSince}
          sparkUntil={dash.sparkUntil}
          onselect={(source, metric) => dash.select(source, metric)}
        />
      {/each}
    </section>
  {:else if status}
    <p class="none">The daemon has not reported any readings yet.</p>
  {/if}

  <HistoryChart {dash} />
  <EventsTable events={dash.events} />

  <footer>{footer}</footer>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    gap: 20px;
    max-width: 1040px;
    margin: 0 auto;
    padding: 24px 16px 40px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: 16px;
  }
  .none {
    margin: 0;
    color: var(--ink2);
  }
  footer {
    font-size: 12px;
    color: var(--muted);
    text-align: center;
  }
</style>
