<script lang="ts">
  import { api, type Group } from '$lib/api';
  import { reloadTree } from '$lib/tree-state.svelte';
  import Modal from './Modal.svelte';

  let { onClose }: { onClose: () => void } = $props();

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

  let groups = $state<Group[]>([]);
  let groupUuids = $state<string[]>([]);
  let groupSearch = $state('');
  let groupPickerOpen = $state(false);
  let groupInputEl: HTMLInputElement | undefined = $state();
  let displayName = $state('');
  let kind = $state<ProbeKind>('ping');
  // Initialised from /settings/hosts once the modal mounts, so admins
  // can pre-pick their preferred cadence without re-typing it for every
  // host. Falls back to the hardcoded defaults if the fetch fails or
  // hasn't returned yet.
  let intervalSecs = $state(60);
  let samplesPerPeriod = $state(20);

  // Advanced section (collapsed by default). chunkWindowMinutes is baked
  // into the host's storage at creation; we can't change it later, so we
  // tuck it away here to avoid scaring off first-time users.
  let advancedOpen = $state(false);
  let chunkWindowMinutes = $state(60);

  let ping = $state({ target: '8.8.8.8', prefer_ipv6: false });
  let dns = $state({ query: 'example.com', record_type: 'A', resolver: '' });
  let tcp = $state({ host: 'example.com', port: 443 });
  let tls = $state({ host: 'example.com', port: 443, sni: '', verify: true });
  // Expected HTTP status codes are stored as an array of chips. Each chip is
  // either an exact 3-digit code (`100..=599`) or one of the class shortcuts
  // `1xx 2xx 3xx 4xx 5xx`. The backend stores them as a comma-joined
  // string; we split/join at the form boundary.
  let httpTtfb = $state({
    url: 'https://example.com',
    method: 'GET',
    expect_statuses: ['2xx'] as string[],
    statusInput: '',
    verify_tls: true,
    follow_redirects: false
  });
  let httpTotal = $state({
    url: 'https://example.com',
    method: 'GET',
    expect_statuses: ['2xx'] as string[],
    statusInput: '',
    verify_tls: true,
    follow_redirects: false,
    max_bytes: 65536
  });

  const DNS_RECORD_TYPES = ['A', 'AAAA', 'MX', 'TXT', 'CNAME', 'NS'] as const;
  const HTTP_METHODS = ['GET', 'HEAD', 'POST'] as const;
  const STATUS_TOKEN = /^(?:[1-5][0-9]{2}|[1-5]xx)$/;

  function isValidStatusToken(t: string): boolean {
    return STATUS_TOKEN.test(t);
  }

  type HttpCfg = typeof httpTtfb | typeof httpTotal;

  /** Commit whatever is in `statusInput` into the chips array, splitting on
   *  commas/whitespace, deduping, and dropping invalid tokens. Returns the
   *  list of rejected tokens so the UI can surface them. */
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
      if (!cfg.expect_statuses.includes(c) && !accepted.includes(c)) {
        accepted.push(c);
      }
    }
    cfg.expect_statuses = [...cfg.expect_statuses, ...accepted];
    cfg.statusInput = rejected.join(', ');
    return rejected;
  }

  function removeStatus(cfg: HttpCfg, code: string) {
    cfg.expect_statuses = cfg.expect_statuses.filter((c) => c !== code);
  }

  let statusErr = $state<string | null>(null);
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

  let busy = $state(false);
  let err = $state<string | null>(null);
  let lastCreated = $state<string | null>(null);

  $effect(() => {
    void api.listGroups().then((g) => (groups = g));
  });

  // Pull operator-configured defaults so they don't have to retype them.
  // We swallow errors (and forbidden for non-admins) since defaults are
  // advisory; the form already has hardcoded fallbacks.
  $effect(() => {
    void api.getHostDefaults().then(
      (res) => {
        intervalSecs = res.defaults.interval_secs;
        samplesPerPeriod = res.defaults.samples_per_period;
      },
      () => {
        /* keep hardcoded defaults */
      }
    );
  });

  function breadcrumb(g: Group): string {
    const byUuid = new Map<string, Group>(groups.map((x) => [x.uuid, x]));
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

  // Live-filter unselected groups by case-insensitive substring on display
  // name or breadcrumb. Cap to 50 results so a typo doesn't try to render
  // 10k DOM nodes - the user can refine with more text.
  const groupMatches = $derived.by(() => {
    const q = groupSearch.trim().toLowerCase();
    const available = groups.filter((g) => !groupUuids.includes(g.uuid));
    if (!q) return available.slice(0, 50);
    return available
      .filter((g) => {
        const hay = `${g.display_name} ${breadcrumb(g)}`.toLowerCase();
        return hay.includes(q);
      })
      .slice(0, 50);
  });

  function onGroupInputKeydown(e: KeyboardEvent) {
    // Backspace on an empty search input pops the last selected chip - a
    // pattern users know from chip inputs in Gmail / Slack / etc.
    if (e.key === 'Backspace' && groupSearch === '' && groupUuids.length > 0) {
      groupUuids = groupUuids.slice(0, -1);
    } else if (e.key === 'Enter' && groupMatches.length > 0) {
      e.preventDefault();
      addGroup(groupMatches[0].uuid);
    } else if (e.key === 'Escape') {
      groupPickerOpen = false;
    }
  }

  function bumpName(name: string): string {
    const m = name.match(/^(.*?) \((\d+)\)$/);
    if (m) return `${m[1]} (${Number(m[2]) + 1})`;
    return `${name} (2)`;
  }

  function buildProbeConfig(): Record<string, unknown> {
    switch (kind) {
      case 'ping':
        return ping.prefer_ipv6
          ? { target: ping.target, prefer_ipv6: true }
          : { target: ping.target };
      case 'dns': {
        const cfg: Record<string, unknown> = {
          query: dns.query,
          record_type: dns.record_type
        };
        if (dns.resolver.trim()) cfg.resolver = dns.resolver.trim();
        return cfg;
      }
      case 'tcp_connect':
        return { host: tcp.host, port: tcp.port };
      case 'tls_connect': {
        const cfg: Record<string, unknown> = {
          host: tls.host,
          port: tls.port,
          verify: tls.verify
        };
        if (tls.sni.trim()) cfg.sni = tls.sni.trim();
        return cfg;
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

  async function doCreate(): Promise<boolean> {
    err = null;
    if (!displayName.trim()) {
      err = 'Name is required.';
      return false;
    }
    const chunkSecs = Math.round(chunkWindowMinutes * 60);
    if (chunkSecs < 60 || chunkSecs > 86_400) {
      err = 'Chunk window must be between 1 minute and 24 hours (1440 min).';
      return false;
    }
    busy = true;
    try {
      await api.createHost({
        group_uuids: groupUuids,
        display_name: displayName.trim(),
        probe_type: kind,
        probe_config: buildProbeConfig(),
        interval_secs: intervalSecs,
        samples_per_period: samplesPerPeriod,
        chunk_window_secs: chunkSecs
      });
      reloadTree();
      lastCreated = displayName.trim();
      return true;
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
      return false;
    } finally {
      busy = false;
    }
  }

  async function createAndClose() {
    if (await doCreate()) onClose();
  }

  async function createAndAddAnother() {
    if (await doCreate()) {
      displayName = bumpName(lastCreated ?? displayName);
    }
  }

  const inputStyle =
    'background: var(--bg); border-color: var(--border); color: var(--fg)';
</script>

<Modal title="Create host" {onClose}>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      void createAndClose();
    }}
    class="space-y-4 text-xs"
  >
    <label class="block">
      <span style="color: var(--muted)">Name</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        bind:value={displayName}
        type="text"
        required
        autofocus
        placeholder="e.g. Google DNS"
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      />
    </label>

    <div>
      <span style="color: var(--muted)">Groups</span>
      <!-- Chip-input picker: type to filter, click a result (or press Enter)
           to add, backspace on the empty field removes the last chip. Built
           for the case where the operator has hundreds or thousands of
           groups and scrolling a flat checklist isn't practical. -->
      <div
        class="relative mt-0.5 rounded border"
        style="border-color: var(--border)"
      >
        <div class="flex flex-wrap items-center gap-1 px-1.5 py-1 min-h-[28px]">
          {#each groupUuids as gu (gu)}
            {@const g = groups.find((x) => x.uuid === gu)}
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
            class="absolute left-0 right-0 top-full mt-0.5 z-10 max-h-48 overflow-y-auto rounded border shadow-md"
            style="background: var(--bg); border-color: var(--border)"
            role="listbox"
          >
            {#each groupMatches as g (g.uuid)}
              <button
                type="button"
                onmousedown={(e) => e.preventDefault()}
                onclick={() => addGroup(g.uuid)}
                class="w-full text-left px-2 py-1 hover:bg-white/5"
                role="option"
                aria-selected="false"
              >
                {breadcrumb(g)}
              </button>
            {/each}
          </div>
        {/if}
      </div>
      <p class="text-[11px] mt-1" style="color: var(--muted)">
        {#if groups.length === 0}
          No groups defined yet - host will live at the tree root.
        {:else}
          Hosts can belong to any number of groups, or none (then they appear at the root).
        {/if}
      </p>
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
        {@const cfg = kind === 'http_ttfb' ? httpTtfb : httpTotal}
        <label class="block mb-2">
          <span style="color: var(--muted)">URL</span>
          <input bind:value={cfg.url} type="url" required
            placeholder="https://example.com/health"
            class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
        </label>
        <div class="grid grid-cols-2 gap-2 mb-2">
          <label class="block">
            <span style="color: var(--muted)">Method</span>
            <select bind:value={cfg.method}
              class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle}>
              {#each HTTP_METHODS as m (m)}
                <option value={m}>{m}</option>
              {/each}
            </select>
          </label>
          <div class="block">
            <span style="color: var(--muted)">Accepted statuses</span>
            <!-- Chip input: type a code (200) or class (2xx), press
                 Enter/space/comma to commit. Multiple accepted statuses
                 mean "match if any one of these". Backspace on an empty
                 input removes the most recent chip. -->
            <div
              class="mt-0.5 rounded border flex flex-wrap items-center gap-1 px-1.5 py-1 min-h-[28px]"
              style="background: var(--bg); border-color: var(--border)"
            >
              {#each cfg.expect_statuses as code (code)}
                <span
                  class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[11px] mono"
                  style="background: var(--border); color: var(--fg)"
                >
                  {code}
                  <button
                    type="button"
                    onclick={() => removeStatus(cfg, code)}
                    class="text-[10px] opacity-60 hover:opacity-100"
                    aria-label="Remove status"
                  >
                    ✕
                  </button>
                </span>
              {/each}
              <input
                bind:value={cfg.statusInput}
                type="text"
                placeholder={cfg.expect_statuses.length === 0 ? '2xx, 200, 301…' : ''}
                onkeydown={(e) => onStatusKeydown(e, cfg)}
                onblur={() => {
                  const r = flushStatusInput(cfg);
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
                Press Enter / comma / space to add. Match passes if ANY chip matches.
              </span>
            {/if}
          </div>
        </div>
        <div class="flex flex-wrap gap-4 mb-2">
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={cfg.verify_tls} />
            <span>Verify TLS</span>
          </label>
          <label class="flex items-center gap-2">
            <input type="checkbox" bind:checked={cfg.follow_redirects} />
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
        <input bind:value={samplesPerPeriod} type="number" min="1" max="100"
          class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle} />
      </label>
    </div>

    <!-- ── Advanced (collapsed by default) ───────────────────────── -->
    <div class="rounded border" style="border-color: var(--border)">
      <button
        type="button"
        onclick={() => (advancedOpen = !advancedOpen)}
        class="w-full flex items-center gap-2 px-2 py-1.5 text-left"
        style="color: var(--muted)"
      >
        <span class="inline-block w-3 text-center">{advancedOpen ? '▾' : '▸'}</span>
        <span class="text-[11px] uppercase tracking-wider">Advanced</span>
      </button>
      {#if advancedOpen}
        <div class="px-3 pb-3 pt-1 border-t" style="border-color: var(--border)">
          <label class="block">
            <span style="color: var(--muted)">HZC chunk window (minutes)</span>
            <input
              bind:value={chunkWindowMinutes}
              type="number"
              min="1"
              max="1440"
              step="1"
              class="w-full mt-0.5 px-2 py-1 rounded border mono"
              style={inputStyle}
            />
            <span class="block text-[10px] mt-1" style="color: var(--muted)">
              Length of each rolled-up chunk file in the host's HZC storage.
              <strong>Baked in at creation</strong> - can't be changed later, since the value
              lands in the host's <code>meta.json</code> on disk. Shorter = more files but
              bounded WAL on crash. Longer = better compression. Range queries are unaffected.
              Default 60 min (1 h). Max 1440 (24 h).
            </span>
          </label>
        </div>
      {/if}
    </div>

    {#if err}
      <p style="color: var(--latency-bad)">{err}</p>
    {/if}

    <div class="flex justify-end gap-2 pt-1">
      <button
        type="button"
        onclick={onClose}
        class="px-3 py-1 rounded border"
        style="border-color: var(--border)"
      >
        Cancel
      </button>
      <button
        type="button"
        disabled={busy}
        onclick={() => void createAndAddAnother()}
        class="px-3 py-1 rounded border"
        style="border-color: var(--border); opacity: {busy ? 0.6 : 1}"
      >
        Create + add another
      </button>
      <button
        type="submit"
        disabled={busy}
        class="px-3 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {busy ? 0.6 : 1}"
      >
        {busy ? 'Creating…' : 'Create'}
      </button>
    </div>
  </form>
</Modal>
