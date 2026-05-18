<script lang="ts">
  import { api, ApiError, type Group, type Host } from '$lib/api';
  import { reloadTree } from '$lib/tree-state.svelte';
  import Modal from './Modal.svelte';

  let {
    host,
    allGroups,
    onClose
  }: {
    host: Host;
    allGroups: Group[];
    onClose: () => void;
  } = $props();

  type ProbeKind =
    | 'ping'
    | 'dns'
    | 'tcp_connect'
    | 'tls_connect'
    | 'http_ttfb'
    | 'http_total';

  const PROBE_OPTIONS: { kind: ProbeKind; label: string; description: string }[] = [
    { kind: 'ping',        label: 'PING',        description: 'ICMP echo round-trip time.' },
    { kind: 'dns',         label: 'DNS',         description: 'Resolution latency for a query.' },
    { kind: 'tcp_connect', label: 'TCP CONNECT', description: 'TCP handshake time to host:port.' },
    { kind: 'tls_connect', label: 'TLS CONNECT', description: 'TCP + TLS handshake.' },
    { kind: 'http_ttfb',   label: 'HTTP TTFB',   description: 'Time to first byte.' },
    { kind: 'http_total',  label: 'HTTP TOTAL',  description: 'Full request including body.' }
  ];

  const DNS_RECORD_TYPES = ['A', 'AAAA', 'MX', 'TXT', 'CNAME', 'NS'] as const;
  const HTTP_METHODS = ['GET', 'HEAD', 'POST'] as const;
  const STATUS_TOKEN = /^(?:[1-5][0-9]{2}|[1-5]xx)$/;

  // ─── Init from the existing host snapshot ────────────────────────────────
  // Each probe kind has its own form state; pre-populating the ones the
  // user isn't currently editing lets them switch probe_type and still
  // see sensible defaults for the new kind. The active kind's fields are
  // seeded from the actual probe_config so the form opens with what's
  // already running.
  // svelte-ignore state_referenced_locally
  const cfg = (host.probe_config ?? {}) as Record<string, unknown>;
  const asString = (v: unknown, fb: string) => (typeof v === 'string' ? v : fb);
  const asNumber = (v: unknown, fb: number) => (typeof v === 'number' ? v : fb);
  const asBool = (v: unknown, fb: boolean) => (typeof v === 'boolean' ? v : fb);
  const initialStatuses = asString(cfg.expect_status, '2xx')
    .split(',')
    .map((s: string) => s.trim())
    .filter((s: string) => s.length > 0);

  // svelte-ignore state_referenced_locally
  let displayName = $state(host.display_name);
  // svelte-ignore state_referenced_locally
  let groupUuids = $state<string[]>([...host.group_uuids]);
  let groupSearch = $state('');
  let groupPickerOpen = $state(false);
  let groupInputEl: HTMLInputElement | undefined = $state();
  let groupListEl: HTMLDivElement | undefined = $state();
  let groupHighlighted = $state(0);

  // svelte-ignore state_referenced_locally
  let kind = $state<ProbeKind>(host.probe_type as ProbeKind);
  // svelte-ignore state_referenced_locally
  let intervalSecs = $state(host.interval_secs);
  // svelte-ignore state_referenced_locally
  let samplesPerPeriod = $state(host.samples_per_period);

  let ping = $state({
    target: asString(cfg.target, '8.8.8.8'),
    prefer_ipv6: asBool(cfg.prefer_ipv6, false)
  });
  let dns = $state({
    query: asString(cfg.query, 'example.com'),
    record_type: asString(cfg.record_type, 'A'),
    resolver: asString(cfg.resolver, '')
  });
  let tcp = $state({
    host: asString(cfg.host, 'example.com'),
    port: asNumber(cfg.port, 443)
  });
  let tls = $state({
    host: asString(cfg.host, 'example.com'),
    port: asNumber(cfg.port, 443),
    sni: asString(cfg.sni, ''),
    verify: asBool(cfg.verify, true)
  });
  let httpTtfb = $state({
    url: asString(cfg.url, 'https://example.com'),
    method: asString(cfg.method, 'GET'),
    expect_statuses: [...initialStatuses],
    statusInput: '',
    verify_tls: asBool(cfg.verify_tls, true),
    follow_redirects: asBool(cfg.follow_redirects, false)
  });
  let httpTotal = $state({
    url: asString(cfg.url, 'https://example.com'),
    method: asString(cfg.method, 'GET'),
    expect_statuses: [...initialStatuses],
    statusInput: '',
    verify_tls: asBool(cfg.verify_tls, true),
    follow_redirects: asBool(cfg.follow_redirects, false),
    max_bytes: asNumber(cfg.max_bytes, 65_536)
  });

  let busy = $state(false);
  let err = $state<string | null>(null);
  let statusErr = $state<string | null>(null);

  // ─── Group chip picker ───────────────────────────────────────────────────
  function breadcrumb(g: Group): string {
    const byUuid = new Map<string, Group>(allGroups.map((x) => [x.uuid, x]));
    const parts: string[] = [];
    let cur: Group | undefined = g;
    while (cur) {
      parts.unshift(cur.display_name);
      cur = cur.parent_uuid != null ? byUuid.get(cur.parent_uuid) : undefined;
    }
    return parts.join(' > ');
  }
  function addGroup(uuid: string) {
    if (!groupUuids.includes(uuid)) groupUuids = [...groupUuids, uuid];
    groupSearch = '';
    groupPickerOpen = false;
    groupInputEl?.focus();
  }
  function removeGroup(uuid: string) {
    groupUuids = groupUuids.filter((g) => g !== uuid);
  }
  const groupMatches = $derived.by(() => {
    const q = groupSearch.trim().toLowerCase();
    const available = allGroups.filter((g) => !groupUuids.includes(g.uuid));
    if (!q) return available.slice(0, 50);
    return available
      .filter((g) => {
        const hay = `${g.display_name} ${breadcrumb(g)}`.toLowerCase();
        return hay.includes(q);
      })
      .slice(0, 50);
  });
  $effect(() => {
    void groupMatches.length;
    groupHighlighted = 0;
  });
  $effect(() => {
    if (!groupPickerOpen) return;
    const i = groupHighlighted;
    queueMicrotask(() => {
      const el = groupListEl?.children[i] as HTMLElement | undefined;
      el?.scrollIntoView({ block: 'nearest' });
    });
  });

  function onGroupInputKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && groupPickerOpen) {
      groupPickerOpen = false;
      e.stopPropagation();
      e.preventDefault();
      return;
    }
    if (e.key === 'Backspace' && groupSearch === '' && groupUuids.length > 0) {
      groupUuids = groupUuids.slice(0, -1);
    } else if (e.key === 'ArrowDown' && groupMatches.length > 0) {
      e.preventDefault();
      groupHighlighted = (groupHighlighted + 1) % groupMatches.length;
    } else if (e.key === 'ArrowUp' && groupMatches.length > 0) {
      e.preventDefault();
      groupHighlighted =
        (groupHighlighted - 1 + groupMatches.length) % groupMatches.length;
    } else if (e.key === 'Enter' && groupMatches.length > 0) {
      e.preventDefault();
      const idx = Math.min(groupHighlighted, groupMatches.length - 1);
      addGroup(groupMatches[idx].uuid);
    }
  }

  // ─── HTTP status chip picker (same UX as CreateHostModal) ────────────────
  function isValidStatusToken(t: string): boolean {
    return STATUS_TOKEN.test(t);
  }
  type HttpCfg = typeof httpTtfb | typeof httpTotal;
  function flushStatusInput(cfg: HttpCfg): string[] {
    const raw = cfg.statusInput.trim();
    if (!raw) return [];
    const candidates = raw.split(/[,\s]+/).filter((s) => s.length > 0);
    const accepted: string[] = [];
    const rejected: string[] = [];
    for (const c of candidates) {
      if (!isValidStatusToken(c)) {
        rejected.push(c);
        continue;
      }
      if (!cfg.expect_statuses.includes(c) && !accepted.includes(c)) accepted.push(c);
    }
    cfg.expect_statuses = [...cfg.expect_statuses, ...accepted];
    cfg.statusInput = rejected.join(', ');
    return rejected;
  }
  function removeStatus(cfg: HttpCfg, code: string) {
    cfg.expect_statuses = cfg.expect_statuses.filter((c) => c !== code);
  }
  function onStatusKeydown(e: KeyboardEvent, cfg: HttpCfg) {
    if (e.key === 'Enter' || e.key === ',' || e.key === ' ') {
      e.preventDefault();
      const rejected = flushStatusInput(cfg);
      statusErr = rejected.length
        ? `Invalid: ${rejected.join(', ')} (expected 100-599 or 1xx/2xx/3xx/4xx/5xx)`
        : null;
    } else if (e.key === 'Backspace' && cfg.statusInput === '' && cfg.expect_statuses.length > 0) {
      cfg.expect_statuses = cfg.expect_statuses.slice(0, -1);
    }
  }

  // ─── Build probe_config from the active form state ───────────────────────
  function buildProbeConfig(): Record<string, unknown> {
    switch (kind) {
      case 'ping':
        return ping.prefer_ipv6
          ? { target: ping.target, prefer_ipv6: true }
          : { target: ping.target };
      case 'dns': {
        const out: Record<string, unknown> = { query: dns.query, record_type: dns.record_type };
        if (dns.resolver.trim()) out.resolver = dns.resolver.trim();
        return out;
      }
      case 'tcp_connect':
        return { host: tcp.host, port: tcp.port };
      case 'tls_connect': {
        const out: Record<string, unknown> = {
          host: tls.host,
          port: tls.port,
          verify: tls.verify
        };
        if (tls.sni.trim()) out.sni = tls.sni.trim();
        return out;
      }
      case 'http_ttfb': {
        flushStatusInput(httpTtfb);
        return {
          url: httpTtfb.url,
          method: httpTtfb.method,
          expect_status: httpTtfb.expect_statuses.join(','),
          verify_tls: httpTtfb.verify_tls,
          follow_redirects: httpTtfb.follow_redirects
        };
      }
      case 'http_total': {
        flushStatusInput(httpTotal);
        return {
          url: httpTotal.url,
          method: httpTotal.method,
          expect_status: httpTotal.expect_statuses.join(','),
          verify_tls: httpTotal.verify_tls,
          follow_redirects: httpTotal.follow_redirects,
          max_bytes: httpTotal.max_bytes
        };
      }
    }
  }

  // ─── Save ────────────────────────────────────────────────────────────────
  async function save() {
    err = null;
    const name = displayName.trim();
    if (!name) {
      err = 'Name is required.';
      return;
    }
    if (intervalSecs < 1) {
      err = 'Interval must be at least 1 second.';
      return;
    }
    if (samplesPerPeriod < 1 || samplesPerPeriod > 1000) {
      err = 'Samples per period must be between 1 and 1000.';
      return;
    }
    busy = true;

    const newConfig = buildProbeConfig();
    // Send only changed fields. probe_config equality is structural; a
    // simple JSON-stringify compare is good enough for our shapes.
    const patch: {
      display_name?: string;
      group_uuids?: string[];
      probe_type?: string;
      probe_config?: Record<string, unknown>;
      interval_secs?: number;
      samples_per_period?: number;
    } = {};
    if (name !== host.display_name) patch.display_name = name;

    const groupsChanged =
      groupUuids.length !== host.group_uuids.length ||
      groupUuids.some((u) => !host.group_uuids.includes(u));
    if (groupsChanged) patch.group_uuids = groupUuids;

    if (kind !== host.probe_type) patch.probe_type = kind;
    if (JSON.stringify(newConfig) !== JSON.stringify(host.probe_config)) {
      patch.probe_config = newConfig;
    }
    if (intervalSecs !== host.interval_secs) patch.interval_secs = intervalSecs;
    if (samplesPerPeriod !== host.samples_per_period)
      patch.samples_per_period = samplesPerPeriod;

    try {
      if (Object.keys(patch).length > 0) {
        await api.updateHost(host.uuid, patch);
        reloadTree();
      }
      onClose();
    } catch (e) {
      err = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function doDelete() {
    const yes = window.confirm(
      `Delete host "${host.display_name}"?\n\n` +
        'The probe scheduler stops and the host\'s HZC chunks are removed ' +
        'from disk. This cannot be undone.'
    );
    if (!yes) return;
    err = null;
    busy = true;
    try {
      await api.deleteHost(host.uuid);
      reloadTree();
      onClose();
    } catch (e) {
      err = e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  const inputStyle =
    'background: var(--bg); border-color: var(--border); color: var(--fg)';
</script>

<Modal title="Edit host" {onClose}>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      void save();
    }}
    class="space-y-4 text-xs"
  >
    <label class="block">
      <span style="color: var(--muted)">Name</span>
      <input
        bind:value={displayName}
        type="text"
        required
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      />
    </label>

    <div>
      <span style="color: var(--muted)">Groups</span>
      <div class="relative mt-0.5 rounded border" style="border-color: var(--border)">
        <div class="flex flex-wrap items-center gap-1 px-1.5 py-1 min-h-[28px]">
          {#each groupUuids as gu (gu)}
            {@const g = allGroups.find((x) => x.uuid === gu)}
            {#if g}
              <span
                class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[11px]"
                style="background: var(--border); color: var(--fg)"
              >
                {breadcrumb(g)}
                <button
                  type="button"
                  onclick={() => removeGroup(gu)}
                  class="ml-0.5 text-[10px] opacity-60 hover:opacity-100"
                  aria-label="Remove group"
                >
                  ✕
                </button>
              </span>
            {/if}
          {/each}
          <input
            bind:this={groupInputEl}
            bind:value={groupSearch}
            type="text"
            onfocus={() => (groupPickerOpen = true)}
            onkeydown={onGroupInputKeydown}
            placeholder={groupUuids.length === 0
              ? 'Type to add groups (leave empty for root)…'
              : 'Add another…'}
            class="flex-1 min-w-[120px] bg-transparent outline-none"
            style="color: var(--fg)"
          />
        </div>
        {#if groupPickerOpen && groupMatches.length > 0}
          <div
            bind:this={groupListEl}
            class="absolute left-0 right-0 top-full mt-0.5 z-10 max-h-48 overflow-y-auto rounded border shadow-md"
            style="background: var(--bg); border-color: var(--border)"
            role="listbox"
          >
            {#each groupMatches as g, i (g.uuid)}
              <button
                type="button"
                onmousedown={(e) => e.preventDefault()}
                onmouseenter={() => (groupHighlighted = i)}
                onclick={() => addGroup(g.uuid)}
                class="w-full text-left px-2 py-1"
                style:background={i === groupHighlighted
                  ? 'rgba(128, 128, 128, 0.25)'
                  : 'transparent'}
                role="option"
                aria-selected={i === groupHighlighted}
              >
                {breadcrumb(g)}
              </button>
            {/each}
          </div>
        {/if}
      </div>
    </div>

    <div>
      <span style="color: var(--muted)">Probe type</span>
      <div class="mt-0.5 grid grid-cols-2 gap-1.5">
        {#each PROBE_OPTIONS as opt (opt.kind)}
          <label
            class="flex items-start gap-2 px-2 py-1.5 rounded border cursor-pointer"
            style="border-color: var(--border); {kind === opt.kind
              ? 'background: rgba(78, 161, 255, 0.10)'
              : ''}"
          >
            <input
              type="radio"
              name="probe-kind"
              value={opt.kind}
              checked={kind === opt.kind}
              onchange={() => (kind = opt.kind)}
              class="mt-0.5"
            />
            <span class="flex-1">
              <span class="font-semibold mono">{opt.label}</span>
              <span class="block text-[10px]" style="color: var(--muted)">
                {opt.description}
              </span>
            </span>
          </label>
        {/each}
      </div>
      {#if kind !== host.probe_type}
        <p class="text-[10px] mt-1" style="color: var(--latency-warn)">
          ⚠ Switching probe type: existing chunks stay as-is. The series
          will hold mixed semantics across the switch point.
        </p>
      {/if}
    </div>

    <fieldset class="rounded border p-3" style="border-color: var(--border)">
      <legend class="px-1 text-[10px] uppercase tracking-wider" style="color: var(--muted)">
        {kind.replace('_', ' ')} settings
      </legend>

      {#if kind === 'ping'}
        <label class="block mb-2">
          <span style="color: var(--muted)">Target (IP or hostname)</span>
          <input bind:value={ping.target} type="text" required
            class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
        </label>
        <label class="flex items-center gap-2">
          <input type="checkbox" bind:checked={ping.prefer_ipv6} />
          <span>Prefer IPv6</span>
        </label>

      {:else if kind === 'dns'}
        <label class="block mb-2">
          <span style="color: var(--muted)">Query name</span>
          <input bind:value={dns.query} type="text" required
            class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
        </label>
        <div class="grid grid-cols-2 gap-2">
          <label class="block">
            <span style="color: var(--muted)">Record type</span>
            <select bind:value={dns.record_type}
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle}>
              {#each DNS_RECORD_TYPES as t (t)}
                <option value={t}>{t}</option>
              {/each}
            </select>
          </label>
          <label class="block">
            <span style="color: var(--muted)">Resolver (optional)</span>
            <input bind:value={dns.resolver} type="text" placeholder="system default"
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
        </div>

      {:else if kind === 'tcp_connect'}
        <div class="grid grid-cols-3 gap-2">
          <label class="block col-span-2">
            <span style="color: var(--muted)">Host</span>
            <input bind:value={tcp.host} type="text" required
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
          <label class="block">
            <span style="color: var(--muted)">Port</span>
            <input bind:value={tcp.port} type="number" min="1" max="65535" required
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
        </div>

      {:else if kind === 'tls_connect'}
        <div class="grid grid-cols-3 gap-2 mb-2">
          <label class="block col-span-2">
            <span style="color: var(--muted)">Host</span>
            <input bind:value={tls.host} type="text" required
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
          <label class="block">
            <span style="color: var(--muted)">Port</span>
            <input bind:value={tls.port} type="number" min="1" max="65535" required
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
        </div>
        <label class="block mb-2">
          <span style="color: var(--muted)">SNI (optional)</span>
          <input bind:value={tls.sni} type="text" placeholder="defaults to host"
            class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
        </label>
        <label class="flex items-center gap-2">
          <input type="checkbox" bind:checked={tls.verify} />
          <span>Verify certificate</span>
        </label>

      {:else if kind === 'http_ttfb' || kind === 'http_total'}
        {@const httpCfg = kind === 'http_ttfb' ? httpTtfb : httpTotal}
        <label class="block mb-2">
          <span style="color: var(--muted)">URL</span>
          <input bind:value={httpCfg.url} type="url" required
            placeholder="https://example.com/health"
            class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
        </label>
        <div class="grid grid-cols-2 gap-2 mb-2">
          <label class="block">
            <span style="color: var(--muted)">Method</span>
            <select bind:value={httpCfg.method}
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle}>
              {#each HTTP_METHODS as m (m)}
                <option value={m}>{m}</option>
              {/each}
            </select>
          </label>
          <div class="block">
            <span style="color: var(--muted)">Accepted statuses</span>
            <div
              class="mt-0.5 rounded border flex flex-wrap items-center gap-1 px-1.5 py-1 min-h-[28px]"
              style="background: var(--bg); border-color: var(--border)"
            >
              {#each httpCfg.expect_statuses as code (code)}
                <span
                  class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[11px] mono"
                  style="background: var(--border); color: var(--fg)"
                >
                  {code}
                  <button
                    type="button"
                    onclick={() => removeStatus(httpCfg, code)}
                    class="text-[10px] opacity-60 hover:opacity-100"
                    aria-label="Remove status"
                  >
                    ✕
                  </button>
                </span>
              {/each}
              <input
                bind:value={httpCfg.statusInput}
                type="text"
                placeholder={httpCfg.expect_statuses.length === 0 ? '2xx, 200, 301…' : ''}
                onkeydown={(e) => onStatusKeydown(e, httpCfg)}
                onblur={() => {
                  const r = flushStatusInput(httpCfg);
                  statusErr = r.length
                    ? `Invalid: ${r.join(', ')} (expected 100-599 or 1xx/2xx/3xx/4xx/5xx)`
                    : null;
                }}
                class="flex-1 min-w-[80px] bg-transparent outline-none mono text-[11px]"
                style="color: var(--fg)"
              />
            </div>
            {#if statusErr}
              <span class="text-[10px]" style="color: var(--latency-bad)">{statusErr}</span>
            {:else}
              <span class="text-[10px]" style="color: var(--muted)">
                Enter / comma / space adds. Match passes if ANY chip matches.
              </span>
            {/if}
          </div>
        </div>
        <div class="flex flex-wrap gap-4 mb-2">
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={httpCfg.verify_tls} />
            <span>Verify TLS</span>
          </label>
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={httpCfg.follow_redirects} />
            <span>Follow redirects</span>
          </label>
        </div>
        {#if kind === 'http_total'}
          <label class="block">
            <span style="color: var(--muted)">Max bytes</span>
            <input bind:value={httpTotal.max_bytes} type="number" min="0"
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
          </label>
        {/if}
      {/if}
    </fieldset>

    <div class="grid grid-cols-2 gap-2">
      <label class="block">
        <span style="color: var(--muted)">Interval (s)</span>
        <input bind:value={intervalSecs} type="number" min="1"
          class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
      </label>
      <label class="block">
        <span style="color: var(--muted)">Samples per period</span>
        <input bind:value={samplesPerPeriod} type="number" min="1" max="1000"
          class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
      </label>
    </div>

    <div class="rounded border p-2 text-[11px]" style="border-color: var(--border); color: var(--muted)">
      Chunk window ({host.chunk_window_secs / 60} min) was baked in when this host
      was created and can't be changed without re-creating the host.
      The scheduler restarts the probe whenever you save - changes apply
      on the next probe period.
    </div>

    {#if err}
      <p style="color: var(--latency-bad)">{err}</p>
    {/if}

    <div class="flex items-center justify-between gap-2 pt-1">
      <button
        type="button"
        onclick={doDelete}
        disabled={busy}
        class="px-3 py-1 rounded border"
        style="border-color: var(--latency-bad); color: var(--latency-bad); opacity: {busy ? 0.6 : 1}"
      >
        Delete
      </button>
      <div class="flex gap-2">
        <button
          type="button"
          onclick={onClose}
          class="px-3 py-1 rounded border"
          style="border-color: var(--border)"
        >
          Cancel
        </button>
        <button
          type="submit"
          disabled={busy}
          class="px-3 py-1 rounded font-medium"
          style="background: var(--btn-bg); color: var(--btn-text); opacity: {busy ? 0.6 : 1}"
        >
          {busy ? 'Saving…' : 'Save'}
        </button>
      </div>
    </div>
  </form>
</Modal>
