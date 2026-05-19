<script lang="ts">
  import { page } from '$app/state';
  import { untrack } from 'svelte';
  import { api, type Group, type Host, type StorageSettings } from '$lib/api';
  import HostChartCard from '$lib/components/HostChartCard.svelte';
  import { fmt, partsInZone } from '$lib/timezone.svelte';

  let groupUuid = $derived(page.params.uuid);
  let group = $state<Group | null>(null);
  let hosts = $state<Host[]>([]);
  let storage = $state<StorageSettings | null>(null);
  let loadErr = $state<string | null>(null);

  // ─── Window controls (mirrors host detail page) ──────────────────────────
  type Preset = { label: string; spanSecs: number };
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

  let preset = $state<Preset>(PRESETS[0]);
  let toSecs = $state<number>(Math.floor(Date.now() / 1000));
  let fromSecs = $derived.by(() => {
    if (preset.label === 'max') {
      // Earliest created_at among the loaded hosts caps the window: there
      // can't be data older than that for any chart in view. Falls back to
      // the retention horizon when hosts haven't loaded yet.
      const horizon = retentionHorizonSecs();
      let from = toSecs - horizon;
      const earliest = earliestHostCreated();
      if (earliest != null && earliest > from) from = earliest;
      return from;
    }
    return toSecs - preset.spanSecs;
  });

  function retentionHorizonSecs(): number {
    if (!storage || storage.retention_tiers.length === 0) {
      return 5 * 365 * 86_400;
    }
    return Math.max(...storage.retention_tiers.map((t) => t.max_age_secs));
  }

  function earliestHostCreated(): number | null {
    if (hosts.length === 0) return null;
    return Math.min(...hosts.map((h) => h.created_at));
  }

  let live = $state(true);
  let nowTick = $state<number>(Math.floor(Date.now() / 1000));
  // Bumped per refresh window; child cards subscribe so the parent owns
  // the refresh cadence and we don't have N cards each running their own
  // setInterval.
  let refreshTick = $state(0);

  $effect(() => {
    if (!live) return;
    const t = setInterval(() => {
      nowTick = Math.floor(Date.now() / 1000);
    }, 1_000);
    return () => clearInterval(t);
  });

  $effect(() => {
    if (!live) return;
    toSecs = nowTick;
  });

  $effect(() => {
    if (!live) return;
    const i = setInterval(() => (refreshTick += 1), 5_000);
    return () => clearInterval(i);
  });

  async function loadGroupAndHosts() {
    if (!groupUuid) return;
    loadErr = null;
    try {
      const [g, hs, st] = await Promise.all([
        api.getGroup(groupUuid),
        api.listHosts({ subtreeOf: groupUuid }),
        api.getStorageSettings().catch(() => null)
      ]);
      group = g;
      hosts = hs;
      if (st) storage = st;
    } catch (e) {
      loadErr = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    const u = groupUuid;
    if (!u) return;
    untrack(() => {
      group = null;
      hosts = [];
      loadErr = null;
      void loadGroupAndHosts();
    });
  });

  function selectPreset(p: Preset) {
    preset = p;
    live = true;
    toSecs = Math.floor(Date.now() / 1000);
    refreshTick += 1;
  }

  function refreshNow() {
    toSecs = Math.floor(Date.now() / 1000);
    refreshTick += 1;
  }

  function toggleLive() {
    live = !live;
    if (live) refreshNow();
  }

  function onZoom(zoomFrom: number, zoomTo: number) {
    // Same gesture semantics as the host page: drag-zoom on ANY chart
    // updates the shared window for ALL visible charts.
    live = false;
    toSecs = zoomTo;
    preset = { label: 'custom', spanSecs: zoomTo - zoomFrom };
    refreshTick += 1;
  }

  function formatRange(from: number, to: number): string {
    const fromP = partsInZone(from);
    const toP = partsInZone(to);
    const sameDay =
      fromP.year === toP.year && fromP.month === toP.month && fromP.day === toP.day;
    const dOpts: Intl.DateTimeFormatOptions = {
      year: 'numeric',
      month: 'short',
      day: '2-digit'
    };
    const tOpts: Intl.DateTimeFormatOptions = {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false
    };
    const fromDate = fmt(from, dOpts);
    const fromTime = fmt(from, tOpts);
    const toTime = fmt(to, tOpts);
    if (sameDay) return `${fromDate}  ${fromTime} → ${toTime}`;
    return `${fromDate} ${fromTime} → ${fmt(to, dOpts)} ${toTime}`;
  }
</script>

<div class="p-2 md:p-4">
  {#if loadErr}
    <p class="text-xs mb-2" style="color: var(--latency-bad)">{loadErr}</p>
  {/if}

  {#if group}
    <!-- Sticky compact header (title + count + presets + LIVE + time
         range). Same shape as the host page so the two feel coherent.
         The preset strip horizontally scrolls on narrow viewports. -->
    <div
      class="sticky top-0 z-20 -mx-2 md:-mx-4 px-2 md:px-4 pt-2 md:pt-0 pb-2 border-b md:border-none"
      style="background: var(--bg); border-color: var(--border)"
    >
      <header class="mb-2">
        <h1 class="text-sm font-semibold truncate">{group.display_name}</h1>
        <p class="text-[11px]" style="color: var(--muted)">
          {hosts.length} probe{hosts.length === 1 ? '' : 's'} in this group + descendants
        </p>
      </header>

      <div class="flex items-center gap-1 overflow-x-auto -mx-2 px-2 md:mx-0 md:px-0">
        {#each PRESETS as p (p.label)}
          <button
            type="button"
            onclick={() => selectPreset(p)}
            class="px-2 py-0.5 rounded text-xs shrink-0"
            style="background: {preset.label === p.label ? 'var(--accent)' : 'var(--border)'}; color: {preset.label === p.label ? '#0b0d10' : 'var(--fg)'}"
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
        <span class="hidden md:inline ml-auto text-xs mono shrink-0" style="color: var(--muted)">
          {formatRange(fromSecs, toSecs)}
        </span>
      </div>
      <div class="md:hidden mt-1 text-[11px] mono" style="color: var(--muted)">
        {formatRange(fromSecs, toSecs)}
      </div>
    </div>

    {#if hosts.length === 0}
      <p class="text-xs" style="color: var(--muted)">
        No probes in this group or any descendant group.
      </p>
    {:else}
      <div class="space-y-2">
        {#each hosts as host (host.uuid)}
          <HostChartCard {host} {fromSecs} {toSecs} {refreshTick} {onZoom} />
        {/each}
      </div>
    {/if}
  {:else if !loadErr}
    <p class="text-xs" style="color: var(--muted)">Loading…</p>
  {/if}
</div>
