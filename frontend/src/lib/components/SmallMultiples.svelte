<script lang="ts">
  import type { SeriesResp } from '$lib/api';
  import { isAbortError, loadSeries, samplesForWidth } from '$lib/series';
  import SmokeChart from './SmokeChart.svelte';

  let {
    hostUuid,
    onZoom
  }: {
    hostUuid: string;
    /**
     * Called when the user drag-zooms or right-click zooms-out on a panel.
     * Forwarded to the host detail page so the main chart updates - the
     * panels themselves stay anchored to their fixed spans.
     */
    onZoom?: (fromSecs: number, toSecs: number) => void;
  } = $props();

  const PANELS = [
    { label: '6 hours', spanSecs: 6 * 3600 },
    { label: '48 hours', spanSecs: 48 * 3600 },
    { label: '14 days', spanSecs: 14 * 86_400 },
    { label: '90 days', spanSecs: 90 * 86_400 }
  ] as const;

  type Loaded = {
    label: string;
    spanSecs: number;
    fromSecs: number;
    toSecs: number;
    series: SeriesResp | null;
    err: string | null;
  };
  let panels = $state<Loaded[]>(
    PANELS.map((p) => ({
      label: p.label,
      spanSecs: p.spanSecs,
      fromSecs: 0,
      toSecs: 0,
      series: null,
      err: null
    }))
  );

  async function loadAll() {
    const now = Math.floor(Date.now() / 1000);
    // Per-panel sample budget tracks the panel's pixel width. Same formula
    // as the main chart so buckets in the small-multiples render at roughly
    // the same visual density (one bucket per ~6 px).
    const target = samplesForWidth(panelWidth);
    await Promise.all(
      PANELS.map(async (p, i) => {
        const fromSecs = now - p.spanSecs;
        const toSecs = now;
        try {
          const series = await loadSeries({
            hostUuid,
            fromSecs,
            toSecs,
            targetSamples: target
          });
          // Skip the assignment entirely when nothing meaningful changed -
          // the loadSeries cache aligns to bucket boundaries, so if the
          // current bucket-aligned window is unchanged it returns the exact
          // same object reference. Avoiding the reassignment keeps Svelte
          // from re-rendering the SmokeChart, which keeps the page from
          // jumping during the 10-second auto-refresh.
          const prev = panels[i];
          if (
            prev.series === series &&
            prev.fromSecs === fromSecs &&
            prev.toSecs === toSecs
          ) {
            return;
          }
          panels[i] = { label: p.label, spanSecs: p.spanSecs, fromSecs, toSecs, series, err: null };
        } catch (e) {
          // Aborts come from rapid refreshes superseding each other; the
          // winning request will update the panel, so don't render the
          // abort as a per-panel error.
          if (isAbortError(e)) return;
          panels[i] = {
            label: p.label,
            spanSecs: p.spanSecs,
            fromSecs,
            toSecs,
            series: null,
            err: e instanceof Error ? e.message : String(e)
          };
        }
      })
    );
  }

  // Track the grid container's width and derive per-panel width from the
  // column count produced by the CSS grid `auto-fit` rule (minmax 420 px).
  // No per-panel refs required.
  let gridContainer: HTMLDivElement | undefined = $state();
  let panelWidth = $state(600);
  $effect(() => {
    if (!gridContainer) return;
    let pending: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver((entries) => {
      // Mirror the auto-fit rule (minmax 420 px per column) to derive
      // per-panel width from the grid container's width.
      const w = Math.round(entries[0].contentRect.width);
      const cols = Math.max(1, Math.floor(w / 420));
      const next = Math.max(200, Math.round(w / cols));
      if (next === panelWidth) return;
      if (pending) clearTimeout(pending);
      pending = setTimeout(() => {
        panelWidth = next;
        void loadAll();
      }, 200);
    });
    ro.observe(gridContainer);
    return () => {
      ro.disconnect();
      if (pending) clearTimeout(pending);
    };
  });

  $effect(() => {
    if (hostUuid) {
      // Reset each panel to "loading" before the new fetch - keeps the
      // previous host's charts from lingering while switching hosts.
      panels = PANELS.map((p) => ({
        label: p.label,
        spanSecs: p.spanSecs,
        fromSecs: 0,
        toSecs: 0,
        series: null,
        err: null
      }));
      void loadAll();
    }
  });

  // Auto-refresh small-multiples too. They always show "last X" relative to
  // now so there's no zoom state to worry about - every tick advances the
  // window. The series cache absorbs requests that resolve to the same
  // tier-aligned bucket so we only do real network work when new data has
  // landed.
  $effect(() => {
    if (!hostUuid) return;
    const interval = setInterval(() => {
      void loadAll();
    }, 10_000);
    return () => clearInterval(interval);
  });
</script>

<section class="mt-4">
  <h2 class="text-[10px] uppercase tracking-wider mb-1" style="color: var(--muted)">
    Multi-period view
  </h2>
  <div
    bind:this={gridContainer}
    class="grid gap-2"
    style="grid-template-columns: repeat(auto-fit, minmax(min(420px, 100%), 1fr))"
  >
    {#each panels as panel}
      <div class="border rounded p-1 md:p-2" style="border-color: var(--border)">
        <div class="text-[10px] mb-1" style="color: var(--muted)">
          <span>{panel.label}</span>
        </div>
        {#if panel.series}
          <!-- Always render the chart, even with zero samples, so a host with
               only a few minutes of data still shows a 90-day-wide frame with
               the data tucked against the right edge. -->
          <SmokeChart
            series={panel.series}
            xMin={panel.fromSecs}
            xMax={panel.toSecs}
            {onZoom}
            height={140}
          />
        {:else if panel.err}
          <p class="text-xs p-2" style="color: var(--latency-bad)">{panel.err}</p>
        {:else}
          <p class="text-xs p-2" style="color: var(--muted)">Loading…</p>
        {/if}
      </div>
    {/each}
  </div>
</section>
