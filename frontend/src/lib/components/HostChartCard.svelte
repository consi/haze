<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { untrack } from 'svelte';
  import { type Host, type SeriesResp } from '$lib/api';
  import { cancelInflight, isAbortError, loadSeries, samplesForWidth } from '$lib/series';
  import SmokeChart from './SmokeChart.svelte';
  import GraphLoadingSpinner from './GraphLoadingSpinner.svelte';

  let {
    host,
    fromSecs,
    toSecs,
    refreshTick,
    onZoom
  }: {
    host: Host;
    fromSecs: number;
    toSecs: number;
    /** Bumped by the parent on every live-mode tick / refresh cycle. The
     *  card subscribes to it as a fetch trigger so the parent owns the
     *  refresh cadence centrally rather than each card running its own
     *  timer. */
    refreshTick: number;
    onZoom?: (fromSecs: number, toSecs: number) => void;
  } = $props();

  let wrapper: HTMLDivElement | undefined = $state();
  let chartHost: HTMLDivElement | undefined = $state();
  let visible = $state(false);
  let series = $state<SeriesResp | null>(null);
  let loading = $state(false);
  let err = $state<string | null>(null);
  let targetSamples = $state(samplesForWidth(600));
  let seriesRequestId = 0;

  // IntersectionObserver: only fetch + render the SmokeChart when this
  // card is actually on-screen. With hundreds of hosts in a group we
  // don't want a hidden card eating bandwidth or canvas memory.
  //
  // No rootMargin lookahead: the moment a card scrolls past the viewport
  // edge `visible` flips to false, the live-tick effect stops calling
  // refresh, and any in-flight fetch for this host is aborted by
  // `loadSeries`'s inflight controller. A small loading flash on new
  // cards is the acceptable cost for strict deregister-on-out-of-view.
  $effect(() => {
    if (!wrapper) return;
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) visible = e.isIntersecting;
      }
    );
    io.observe(wrapper);
    return () => io.disconnect();
  });

  // Track chart pixel width so the sample budget matches what's on screen.
  $effect(() => {
    if (!chartHost) return;
    let pending: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver((entries) => {
      const w = Math.round(entries[0].contentRect.width);
      const next = samplesForWidth(w);
      if (next === targetSamples) return;
      if (pending) clearTimeout(pending);
      pending = setTimeout(() => (targetSamples = next), 200);
    });
    ro.observe(chartHost);
    return () => {
      ro.disconnect();
      if (pending) clearTimeout(pending);
    };
  });

  // Fetch series only while the card is on-screen. Tracked deps are
  // chosen deliberately:
  //   - visible: re-run when the card scrolls in (or out).
  //   - host.uuid: re-run when the parent swaps which host this card shows.
  //   - refreshTick: re-run every parent refresh cycle (every 5 s in live
  //     mode, plus on preset / zoom).
  //   - targetSamples: re-run when chart pixel width changes (rare).
  // `fromSecs` / `toSecs` are read inside `untrack` because the parent
  // ticks `toSecs` every second in live mode to slide the chart's pinned
  // x-window. If we tracked it the card would refetch every second per
  // visible host, hammering the backend for no visible benefit.
  //
  // Two cadences here:
  //   - `visible` toggling debounces by 200 ms so a fast scroll past
  //     many cards doesn't produce a cancel-storm: cards that briefly
  //     flicker visible never actually fire a request.
  //   - `refreshTick` / `targetSamples` / `host.uuid` fire immediately
  //     because they signal "user action" or "real configuration change".
  $effect(() => {
    const _trigger = [host.uuid, refreshTick, targetSamples];
    void _trigger;
    const wasVisible = visible;
    if (!wasVisible) {
      // Card scrolled out: cancel any in-flight request right away so we
      // don't keep bandwidth flowing for charts the user can't see.
      // Keep `series` around so re-entering shows the prior chart
      // immediately instead of flashing "Loading…".
      cancelInflight(host.uuid);
      return;
    }
    // Visible. Wait a short tick before firing the fetch; if `visible`
    // flips false during the wait, the cleanup below clears the timer
    // and no request goes out - this saves the cancel-restart storm.
    const timer = setTimeout(() => {
      untrack(() => void refresh());
    }, 200);
    return () => clearTimeout(timer);
  });

  async function refresh() {
    const requestId = ++seriesRequestId;
    loading = true;
    err = null;
    try {
      const next = await loadSeries({
        hostUuid: host.uuid,
        fromSecs,
        toSecs,
        targetSamples
      });
      if (requestId === seriesRequestId) series = next;
    } catch (e) {
      if (isAbortError(e)) return;
      if (requestId === seriesRequestId) err = e instanceof Error ? e.message : String(e);
    } finally {
      if (requestId === seriesRequestId) loading = false;
    }
  }

  function openHost() {
    void goto(`${base}/hosts/${host.uuid}`);
  }
</script>

<div
  bind:this={wrapper}
  class="border rounded p-2 overflow-hidden flex flex-col"
  style="border-color: var(--border); height: 299px"
>
  <header class="flex items-center gap-2 mb-1 text-xs" style="color: var(--muted)">
    <button
      type="button"
      onclick={openHost}
      class="font-semibold hover:underline"
      style="color: var(--fg)"
    >
      {host.display_name}
    </button>
    <span class="px-1 rounded text-[10px]" style="background: var(--border); color: var(--fg)">
      {host.probe_type}
    </span>
    <span>·</span>
    <span>every {host.interval_secs}s × {host.samples_per_period} samples</span>
  </header>

  <!-- Locked height. The pin matters because SmokeChart only mounts when
       data lands, and its stats footer adds vertical content. Without a
       fixed-height container, that mount shifts every card below it,
       which flips IntersectionObserver visibility, which schedules new
       loads, which shift more cards... an infinite cancel-storm. The
       overflow-hidden on the outer card protects against the stats lines
       wrapping on narrow widths. -->
  <div bind:this={chartHost} class="relative w-full flex-1 min-h-0">
    {#if !visible}
      <!-- Placeholder while off-screen: keeps layout stable so the
           container scroll height doesn't jump as cards activate. -->
    {:else if err}
      <p class="text-xs p-2" style="color: var(--latency-bad)">{err}</p>
    {:else if series}
      <SmokeChart
        {series}
        xMin={fromSecs}
        xMax={toSecs}
        {onZoom}
        height={182}
        title={host.display_name}
      />
    {/if}
    {#if visible && (loading || (!series && !err))}
      <GraphLoadingSpinner />
    {/if}
  </div>
</div>
