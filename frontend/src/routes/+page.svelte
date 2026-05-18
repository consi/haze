<script lang="ts">
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { auth, canSeeAlerts, canSeeSettings, canEditHosts, canEditGroups } from '$lib/auth.svelte';

  // The welcome page is logged-in-only. The layout's onMount redirect only
  // runs on initial mount, so clicking the logo while signed-out used to
  // land here without auth. This effect handles every subsequent visit.
  $effect(() => {
    if (!auth.user) void goto(`${base}/login`);
  });

  // Mirrors `LOSS_PALETTE` in SmokeChart.svelte. Eight discrete buckets,
  // labelled by the packet-loss range they cover.
  const LOSS_LEGEND: { color: string; label: string }[] = [
    { color: '#00cc00', label: '0%' },
    { color: '#00b1ff', label: '0–5%' },
    { color: '#5959ff', label: '5–10%' },
    { color: '#b300b3', label: '10–15%' },
    { color: '#ff5cff', label: '15–25%' },
    { color: '#ff950c', label: '25–50%' },
    { color: '#ff0000', label: '50–95%' },
    { color: '#6b1717', label: '95–100%' }
  ];
</script>

<div class="p-6 max-w-3xl space-y-6">
  <header>
    <h1 class="font-display text-xl" style="font-weight: 700; color: var(--fg)">
      Welcome{auth.user ? `, ${auth.user.username}` : ''}
    </h1>
    <p class="text-xs mt-1" style="color: var(--muted)">
      Haze probes hosts on a schedule, records the latency distribution for each
      probe period, and renders the result as a smoke graph - the chart you
      already know from years of staring at the original.
    </p>
  </header>

  <section>
    <h2 class="text-sm font-semibold mb-2" style="color: var(--fg)">Reading a graph</h2>
    <ul class="text-xs space-y-1.5" style="color: var(--fg)">
      <li>
        <span class="mono" style="font-weight: 600">median line</span>
        <span style="color: var(--muted)">- typical latency for the bucket,
        coloured by packet loss on the segment. Eight discrete buckets:</span>
        <span class="flex flex-wrap items-center gap-x-2 gap-y-1 mt-1 text-[10px] mono">
          {#each LOSS_LEGEND as { color, label }}
            <span class="flex items-center gap-1" style="color: var(--muted)">
              <span class="inline-block w-3 h-3" style="background: {color}"></span>
              {label}
            </span>
          {/each}
        </span>
      </li>
      <li>
        <span class="mono" style="font-weight: 600; color: var(--smoke-inner-text)">inner band (p25 – p75)</span>
        <span style="color: var(--muted)">- where half of the samples landed.</span>
      </li>
      <li>
        <span class="mono" style="font-weight: 600; color: var(--smoke-outer-text)">outer band (p2.5 – p97.5)</span>
        <span style="color: var(--muted)">- the wider tail. Tall bands mean jitter; flat bands mean a steady link.</span>
      </li>
      <li>
        <span class="mono" style="font-weight: 600; color: var(--latency-bad)">red</span>
        <span style="color: var(--muted)">- probe ran but every sample failed (the host was unreachable).</span>
      </li>
      <li>
        <span class="mono" style="font-weight: 600">below each chart</span>
        <span style="color: var(--muted)">- a one-glance summary: avg / max / min / now / sd for median rtt, and avg / max / min / now for packet loss.</span>
      </li>
    </ul>
    <p class="text-xs mt-2" style="color: var(--muted)">
      <span style="color: var(--fg); font-weight: 600">Click and drag</span> across a chart to zoom in.
      <span style="color: var(--fg); font-weight: 600">Right-click</span> to zoom out by one window.
      Hover anywhere for an exact reading and the bucket's full distribution.
    </p>
  </section>

  <section>
    <h2 class="text-sm font-semibold mb-2" style="color: var(--fg)">Sidebar</h2>
    <p class="text-xs mb-1.5" style="color: var(--muted)">
      The tree on the left is your host navigator. Search filters by host
      display name or probe type - each space-separated word must match, so
      <span class="mono" style="color: var(--fg)">google ping</span>
      narrows to ICMP probes that mention "google". Matching subtrees expand
      automatically while you type.
    </p>
    <ul class="text-xs space-y-0.5" style="color: var(--muted)">
      {#if canEditGroups()}
        <li><span class="mono font-semibold" style="color: var(--fg)">G</span> - add a group. Groups can nest; only the display name matters.</li>
      {/if}
      {#if canEditHosts()}
        <li><span class="mono font-semibold" style="color: var(--fg)">H</span> - add a host. Pick one of six probe types: PING, DNS, TCP&nbsp;CONNECT, TLS&nbsp;CONNECT, HTTP&nbsp;TTFB, HTTP&nbsp;TOTAL. Each host can sit at the root or inside a group.</li>
      {/if}
      <li><span class="mono font-semibold" style="color: var(--fg)">+</span> - expand every group.</li>
      <li><span class="mono font-semibold" style="color: var(--fg)">−</span> - collapse every group.</li>
    </ul>
  </section>

  <section>
    <h2 class="text-sm font-semibold mb-2" style="color: var(--fg)">Top bar</h2>
    <ul class="text-xs space-y-0.5" style="color: var(--muted)">
      <li>
        <a href={`${base}/user`} class="mono font-semibold underline" style="color: var(--fg)">{auth.user?.username ?? 'username'}</a>
        - your account. Change your password, register a passkey for password-less
        sign-in, and generate API tokens for scripts/clients.
      </li>
      {#if canSeeAlerts()}
        <li>
          <a href={`${base}/alerting`} class="font-semibold underline" style="color: var(--fg)">alerting</a>
          - rule + notifier management. Threshold-on-loss and threshold-on-latency
          rules fire via webhooks. UI under construction; the engine is already running
          in the background.
        </li>
      {/if}
      {#if canSeeSettings()}
        <li>
          <a href={`${base}/settings`} class="font-semibold underline" style="color: var(--fg)">settings</a>
          - system-wide settings (admin-only).
        </li>
      {/if}
      <li>
        <span class="font-semibold" style="color: var(--fg)">log out</span> - end your session.
      </li>
    </ul>
  </section>

  <section>
    <h2 class="text-sm font-semibold mb-2" style="color: var(--fg)">API</h2>
    <p class="text-xs mb-2" style="color: var(--muted)">
      Every endpoint behind the UI is exposed as a versioned REST API. Use it
      from your scripts, oncall runbooks - whatever consumes JSON.
    </p>
    <ul class="text-xs space-y-0.5" style="color: var(--muted)">
      <li>
        Browse the spec:
        <a href={`${base}/api/docs`} class="mono underline" style="color: var(--fg)">{base}/api/docs</a>
        (Swagger UI).
      </li>
      <li>
        Raw JSON:
        <a href={`${base}/api/openapi.json`} class="mono underline" style="color: var(--fg)">{base}/api/openapi.json</a>
        - feed it to any OpenAPI client generator.
      </li>
      <li>
        Authenticate with a personal access token created on the
        <a href={`${base}/user`} class="underline" style="color: var(--fg)">account page</a>:
        <span class="mono" style="color: var(--fg)">Authorization: Bearer hzt_…</span>
      </li>
    </ul>
  </section>

  <footer class="pt-2 text-[11px] space-y-2" style="color: var(--muted); border-top: 1px solid var(--border)">
    <p>
      Pick a host from the left to see it in action. New hosts start filling in
      as soon as their first probe period completes.
    </p>
    <p class="text-[10px]">
      Built by Marek&nbsp;Wajdzik -
      <a
        href="https://github.com/consi"
        target="_blank"
        rel="noopener noreferrer"
        class="underline hover:text-[var(--fg)]"
      >github.com/consi</a>
      ·
      <a
        href="https://linkedin.com/in/marek-wajdzik"
        target="_blank"
        rel="noopener noreferrer"
        class="underline hover:text-[var(--fg)]"
      >linkedin.com/in/marek-wajdzik</a>
    </p>
  </footer>
</div>
