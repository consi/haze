<script lang="ts">
  import { page } from '$app/state';
  import { untrack } from 'svelte';
  import { api, type Host, type SeriesResp, type StorageSettings } from '$lib/api';
  import { isAbortError, loadSeries, samplesForWidth } from '$lib/series';
  import RouteHistoryButton from '$lib/components/RouteHistoryButton.svelte';
  import SmokeChart from '$lib/components/SmokeChart.svelte';
  import GraphLoadingSpinner from '$lib/components/GraphLoadingSpinner.svelte';
  import { fmt, partsInZone } from '$lib/timezone.svelte';
  import { loadGraphRange, saveGraphRange } from '$lib/time-range';

  let hostUuid = $derived(page.params.uuid);
  let host = $state<Host | null>(null);
  let storage = $state<StorageSettings | null>(null);
  let err = $state<string | null>(null);
  // Sample budget derived from chart pixel width via `samplesForWidth`. The
  // formula targets ~6 px per bucket so the visible cells render roughly as
  // squares (matching the 5 px median-line thickness) instead of as narrow
  // 1 px slivers.
  let chartWrapper: HTMLDivElement | undefined = $state();
  let targetSamples = $state(samplesForWidth(800));

  type Preset = { label: string; spanSecs: number };
  // `max` is a sentinel - its actual span is `toSecs - host.created_at`,
  // resolved in the `fromSecs` derivation. The placeholder value is a
  // sensible fallback used when the host record hasn't loaded yet.
  const PRESETS: Preset[] = [
    { label: '30m', spanSecs: 30 * 60 },
    { label: '3h', spanSecs: 3 * 3600 },
    { label: '12h', spanSecs: 12 * 3600 },
    { label: '24h', spanSecs: 24 * 3600 },
    { label: '7d', spanSecs: 7 * 86_400 },
    { label: '30d', spanSecs: 30 * 86_400 },
    { label: '1y', spanSecs: 365 * 86_400 },
    { label: '5y', spanSecs: 5 * 365 * 86_400 },
    { label: 'max', spanSecs: 10 * 365 * 86_400 }
  ];
  const PRESET_LABELS = new Set(PRESETS.map((p) => p.label));

  let preset = $state<Preset>(PRESETS[0]);
  let toSecs = $state<number>(Math.floor(Date.now() / 1000));
  let fixedFromSecs = $state<number | null>(null);
  let fromSecs = $derived.by(() => {
    if (fixedFromSecs != null) return fixedFromSecs;
    if (preset.label === 'max') {
      // "max" = the storage retention horizon (the largest `max_age_secs`
      // across the configured retention tiers). Older data has been deleted
      // by the compactor, so asking for anything further back just shows
      // empty space. Cap to host.created_at: no data ever existed before it.
      const horizon = retentionHorizonSecs();
      let from = toSecs - horizon;
      if (host && host.created_at > from) from = host.created_at;
      return from;
    }
    return toSecs - preset.spanSecs;
  });

  function retentionHorizonSecs(): number {
    if (!storage || storage.retention_tiers.length === 0) {
      // Fallback while storage settings are still loading: 5 years matches
      // the default last tier so the user doesn't see the chart jump when
      // the real value arrives.
      return 5 * 365 * 86_400;
    }
    return Math.max(
      ...storage.retention_tiers.map((t) => t.max_age_secs)
    );
  }
  let series = $state<SeriesResp | null>(null);
  let loading = $state(false);
  let seriesRequestId = 0;

  async function loadHost() {
    if (!hostUuid) return;
    try {
      host = await api.getHost(hostUuid);
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
    }
  }

  async function loadStorageSettings() {
    // Storage settings drive the "max" preset's lower bound. The endpoint
    // is gated on "any logged-in user", not admin, so this is safe to call
    // from the host detail page regardless of role.
    try {
      storage = await api.getStorageSettings();
    } catch {
      // Non-fatal: the fallback retention horizon will be used.
    }
  }

  async function refreshSeries() {
    if (!hostUuid) return;
    const requestId = ++seriesRequestId;
    loading = true;
    err = null;
    try {
      const next = await loadSeries({ hostUuid, fromSecs, toSecs, targetSamples });
      if (requestId === seriesRequestId) series = next;
    } catch (e) {
      // A newer refresh (live tick, zoom, preset) aborted this one - the
      // newer call will land and update state, so we silently drop the
      // abort instead of flashing an error.
      if (isAbortError(e)) return;
      if (requestId === seriesRequestId) err = e instanceof Error ? e.message : String(e);
    } finally {
      if (requestId === seriesRequestId) loading = false;
    }
  }

  function refreshNow() {
    // A manual refresh of a paused/zoomed chart means "reload this exact
    // historical window", not "jump back to now".
    if (live) toSecs = Math.floor(Date.now() / 1000);
    void refreshSeries();
  }

  function persistRange(uuid = hostUuid) {
    if (!uuid) return;
    if (live) {
      saveGraphRange('host', uuid, { version: 1, mode: 'live', preset: preset.label });
      return;
    }
    saveGraphRange('host', uuid, {
      version: 1,
      mode: 'fixed',
      fromSecs,
      toSecs,
      preset: PRESET_LABELS.has(preset.label) ? preset.label : null
    });
  }

  function restoreRange(uuid: string) {
    const restored = loadGraphRange('host', uuid, PRESET_LABELS);
    if (!restored) {
      preset = PRESETS[0];
      fixedFromSecs = null;
      toSecs = Math.floor(Date.now() / 1000);
      live = true;
      persistRange(uuid);
      return;
    }
    if (restored.mode === 'live') {
      preset = PRESETS.find((p) => p.label === restored.preset) ?? PRESETS[0];
      fixedFromSecs = null;
      toSecs = Math.floor(Date.now() / 1000);
      live = true;
    } else {
      const restoredPreset = restored.preset
        ? PRESETS.find((p) => p.label === restored.preset)
        : undefined;
      preset = restoredPreset ?? {
        label: 'custom',
        spanSecs: restored.toSecs - restored.fromSecs
      };
      fixedFromSecs = restored.fromSecs;
      toSecs = restored.toSecs;
      live = false;
    }
    // Client-side navigation inherits the prior page's range; save that
    // inherited value under the destination UUID for later full reloads.
    persistRange(uuid);
  }

  function selectPreset(p: Preset) {
    preset = p;
    // Picking a preset means "follow now again" - re-anchor toSecs to
    // wall-clock and re-enable live mode so the visible window stays
    // synchronised.
    live = true;
    fixedFromSecs = null;
    refreshNow();
    persistRange();
  }

  function onZoom(zoomFrom: number, zoomTo: number) {
    // Replace the visible window with the dragged selection. Width determines
    // the (virtual) preset; we synthesise a custom span. Disable live since
    // the user is now inspecting a fixed historical window.
    live = false;
    fixedFromSecs = zoomFrom;
    toSecs = zoomTo;
    preset = { label: 'custom', spanSecs: zoomTo - zoomFrom };
    persistRange();
    void refreshSeries();
  }

  function toggleLive() {
    if (live) {
      fixedFromSecs = fromSecs;
      live = false;
      persistRange();
    } else {
      live = true;
      fixedFromSecs = null;
      // Re-anchor to wall clock and grab fresh data immediately so the
      // chart doesn't have a stale right edge until the next 5 s tick.
      refreshNow();
      persistRange();
    }
  }

  // Human-readable display of the currently visible window. Compresses to
  // "HH:MM:SS - HH:MM:SS" when both endpoints fall on the same calendar day
  // (in the configured timezone), and expands to full dates when they don't.
  function formatRange(from: number, to: number): string {
    const fromP = partsInZone(from);
    const toP = partsInZone(to);
    const sameDay =
      fromP.year === toP.year && fromP.month === toP.month && fromP.day === toP.day;
    const dateOpts: Intl.DateTimeFormatOptions = {
      year: 'numeric',
      month: 'short',
      day: '2-digit'
    };
    const timeOpts: Intl.DateTimeFormatOptions = {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false
    };
    const fromDate = fmt(from, dateOpts);
    const fromTime = fmt(from, timeOpts);
    const toTime = fmt(to, timeOpts);
    if (sameDay) return `${fromDate}  ${fromTime} → ${toTime}`;
    const toDate = fmt(to, dateOpts);
    return `${fromDate} ${fromTime} → ${toDate} ${toTime}`;
  }

  $effect(() => {
    // Read hostUuid at the top so it's the ONLY tracked dependency.
    // Everything inside `untrack` is invisible to Svelte's reactivity graph
    // - otherwise `refreshSeries()` would synchronously read fromSecs /
    // toSecs / targetSamples while building its request, making this effect
    // re-fire on every live tick and null out `series`.
    const u = hostUuid;
    if (!u) return;
    untrack(() => {
      restoreRange(u);
      host = null;
      series = null;
      err = null;
      void loadHost();
      void loadStorageSettings();
      void refreshSeries();
    });
  });

  // Resize observer for the chart wrapper. Updates `targetSamples` whenever
  // the visible width changes by a meaningful amount; debounced so frequent
  // window-drag events don't spam fetches. The series cache absorbs the noise
  // when a width change doesn't cross a bucket boundary.
  $effect(() => {
    if (!chartWrapper) return;
    let pending: ReturnType<typeof setTimeout> | undefined;
    const ro = new ResizeObserver((entries) => {
      const w = Math.round(entries[0].contentRect.width);
      const next = samplesForWidth(w);
      if (next === targetSamples) return;
      if (pending) clearTimeout(pending);
      pending = setTimeout(() => {
        targetSamples = next;
      }, 200);
    });
    ro.observe(chartWrapper);
    return () => {
      ro.disconnect();
      if (pending) clearTimeout(pending);
    };
  });

  // Re-fetch the series whenever the chosen sample budget changes, but only
  // after the host has loaded. The series cache short-circuits requests that
  // resolve to the same bucket-aligned range. Same untrack pattern as above:
  // refreshSeries reads other state internally, but we don't want this effect
  // to re-fire on every live tick.
  $effect(() => {
    const u = hostUuid;
    const target = targetSamples;
    if (!u || target <= 0) return;
    untrack(() => {
      void refreshSeries();
    });
  });

  // Live mode: when the user is looking at a preset window (anchored at
  // "now"), `live` is on. The visible clock ticks every second so users see
  // the right edge advance in real time, and a slower interval re-fetches
  // the series so new samples appear as the prober writes them.
  //
  // The user can toggle live off explicitly with the pin button; it also
  // turns off automatically whenever they switch to a custom (zoomed)
  // window, since that's clearly inspecting historical data.
  let live = $state(true);

  // Drives the right-edge clock. Bound to `toSecs` via the effect below so
  // the chart always renders "now" at the right edge while live is on.
  let nowTick = $state<number>(Math.floor(Date.now() / 1000));
  $effect(() => {
    if (!live) return;
    const t = setInterval(() => {
      nowTick = Math.floor(Date.now() / 1000);
    }, 1_000);
    return () => clearInterval(t);
  });

  // Push the ticking clock into `toSecs` while live. `$effect` re-runs when
  // `nowTick` or `live` change, so the chart's pinned x-window slides
  // forward each second without re-fetching.
  $effect(() => {
    if (!live) return;
    toSecs = nowTick;
  });

  // Periodic re-fetch on a coarser cadence than the visual clock - one
  // request per chart redraw is enough; we don't need to hammer the API
  // every second. 5 s gives crisp follow-along without thundering.
  $effect(() => {
    if (!hostUuid) return;
    if (!live) return;
    const interval = setInterval(() => {
      void refreshSeries();
    }, 5_000);
    return () => clearInterval(interval);
  });
</script>

<div class="p-2 md:p-4">
  {#if err}
    <p class="text-xs mb-2" style="color: var(--latency-bad)">{err}</p>
  {/if}

  {#if host}
    <!-- Sticky compact header. Row 1: title + meta. Row 2: presets +
         refresh + LIVE. Row 3 (mobile only): time-range pill below the
         preset row so it doesn't fight the LIVE pill for space. The
         whole block stays at the top of the scroll area so controls are
         always at the user's fingertip while scrolling charts. -->
    <div
      class="sticky top-0 z-20 -mx-2 md:-mx-4 px-2 md:px-4 pt-2 md:pt-0 pb-2 border-b md:border-none"
      style="background: var(--bg); border-color: var(--border)"
    >
      <header class="mb-2">
        <h1 class="text-sm font-semibold truncate">{host.display_name}</h1>
        <div class="text-xs flex items-center gap-2 mt-0.5 flex-wrap" style="color: var(--muted)">
          <span class="px-1 rounded" style="background: var(--border); color: var(--fg)">
            {host.probe_type}
          </span>
          <span>·</span>
          <span>every {host.interval_secs}s × {host.samples_per_period} samples</span>
        </div>
      </header>

      <div class="flex items-center gap-1 overflow-x-auto -mx-2 px-2 md:mx-0 md:px-0">
        {#each PRESETS as p}
          <button
            type="button"
            onclick={() => selectPreset(p)}
            class="px-2 py-0.5 rounded text-xs shrink-0"
            style="background: {preset === p ? 'var(--accent)' : 'var(--border)'}; color: {preset === p ? '#0b0d10' : 'var(--fg)'}"
          >
            {p.label}
          </button>
        {/each}
        <button
          type="button"
          onclick={refreshNow}
          class="ml-2 px-2 py-0.5 rounded text-xs shrink-0"
          style="background: var(--border); color: var(--fg)"
          title="Refresh now"
        >
          ↻
        </button>
        <button
          type="button"
          onclick={toggleLive}
          class="px-2 py-0.5 rounded text-xs flex items-center gap-1 shrink-0"
          style="background: {live ? 'var(--latency-good)' : 'var(--border)'}; color: {live ? '#0b0d10' : 'var(--fg)'}"
          title={live ? 'Pause live follow' : 'Resume live follow'}
        >
          <span
            class="inline-block rounded-full"
            style="width: 6px; height: 6px; background: {live ? '#0b0d10' : 'var(--muted)'}; {live ? 'animation: haze-pulse 1.2s ease-in-out infinite' : ''}"
          ></span>
          {live ? 'LIVE' : 'PAUSED'}
        </button>
        <!-- Time-range display: inline on the right on desktop; on mobile
             it would be cramped next to the LIVE pill so we drop it onto
             the next line (see below) and hide this copy. -->
        <span class="hidden md:inline ml-auto text-xs mono shrink-0" style="color: var(--muted)">
          {formatRange(fromSecs, toSecs)}
          {#if loading}
            <span class="ml-2 text-[10px]">loading…</span>
          {/if}
        </span>
      </div>
      <!-- Mobile-only row 3: time-range pill under the preset strip. -->
      <div class="md:hidden mt-1 text-[11px] mono" style="color: var(--muted)">
        {formatRange(fromSecs, toSecs)}
        {#if loading}
          <span class="ml-2 text-[10px]">loading…</span>
        {/if}
      </div>
    </div>

    <div
      bind:this={chartWrapper}
      class="relative border rounded p-1 md:p-2 mt-2 min-h-[276px]"
      style="border-color: var(--border)"
    >
      <div class="flex justify-end mb-1"><RouteHistoryButton {host} {fromSecs} {toSecs}/></div>
      {#if series}
        <!-- Pin the chart's x-axis to the requested window so zoom-out past
             the available data still draws the full window with empty space
             on the left, and so zoom-in to a custom range exactly matches
             what the user dragged. -->
        <SmokeChart
          {series}
          {onZoom}
          xMin={fromSecs}
          xMax={toSecs}
          height={260}
          title={host?.display_name ?? 'host'}
        />
      {:else if !loading}
        <p class="text-xs p-4" style="color: var(--muted)">
          Waiting for the first probe to complete - data will appear after one probe interval.
        </p>
      {/if}
      {#if loading}
        <GraphLoadingSpinner />
      {/if}
    </div>

  {:else}
    <p class="text-xs" style="color: var(--muted)">Loading…</p>
  {/if}

</div>
