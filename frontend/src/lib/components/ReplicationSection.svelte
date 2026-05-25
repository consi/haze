<script lang="ts">
  // Replication settings: three paginated tables (Peers, Rules, Inbound).
  // Lives as one component because the three lists share styling and the
  // sub-tables don't justify three separate files.
  //
  // Live updates:
  //  - SSE `change: replication` triggers a refetch of all three lists
  //    (cheap; pages of 20 + headers).
  //  - A 1 s `setInterval` ticks the lag counters locally so the
  //    `now() - latest_ingested_ts` figure stays current between refetches.
  //  - Worker fires `ChangeKind::Replication` on each ack flush (~30 s),
  //    so the latest_ingested_ts itself updates without a manual refresh.
  import { onMount, onDestroy } from 'svelte';
  import {
    api,
    type ReplicationPeer,
    type ReplicationRule,
    type ReplicationInboundSlot,
    type GroupPreview,
    type Group
  } from '$lib/api';
  import { reloadKeys } from '$lib/events.svelte';
  import ReplicationTopology from './ReplicationTopology.svelte';

  const PAGE_SIZE = 20;

  let myInstanceUuid = $state<string>('');
  $effect(() => {
    void api.serverInfo().then((s) => {
      myInstanceUuid = s.instance_uuid;
    });
  });

  let peersPage = $state(0);
  let peers = $state<ReplicationPeer[]>([]);
  let peersTotal = $state(0);
  let peersLoading = $state(false);
  let peersError = $state<string | null>(null);

  let rulesPage = $state(0);
  let rules = $state<ReplicationRule[]>([]);
  let rulesTotal = $state(0);
  let rulesLoading = $state(false);
  let rulesError = $state<string | null>(null);

  let inboundPage = $state(0);
  let inbound = $state<ReplicationInboundSlot[]>([]);
  let inboundTotal = $state(0);
  let inboundLoading = $state(false);
  let inboundError = $state<string | null>(null);

  // Live tick so the "X seconds ago" lag counters next to each rule
  // refresh in-place without re-fetching the list.
  let nowSecs = $state(Math.floor(Date.now() / 1000));
  let tickHandle: ReturnType<typeof setInterval> | null = null;

  // Add-peer modal state.
  let showAddPeer = $state(false);
  let newPeerName = $state('');
  let newPeerBaseUrl = $state('');
  let newPeerToken = $state('');
  let newPeerSkipTls = $state(false);
  let newPeerError = $state<string | null>(null);
  let newPeerSaving = $state(false);

  // Edit-peer modal state.
  let editingPeer = $state<ReplicationPeer | null>(null);
  let editPeerName = $state('');
  let editPeerToken = $state('');
  let editPeerInterval = $state(300);
  let editPeerSkipTls = $state(false);
  let editPeerError = $state<string | null>(null);
  let editPeerSaving = $state(false);

  // Add-rule modal state.
  let showAddRule = $state(false);
  let newRulePeerUuid = $state('');
  let newRuleSourceGroup = $state<string | null>(null);
  let newRuleDestGroup = $state<string | null>(null);
  let newRulePreviewGroups = $state<GroupPreview[]>([]);
  let newRuleError = $state<string | null>(null);
  let newRuleSaving = $state(false);
  let localGroupsForPicker = $state<Group[]>([]);

  // Peer test state (transient banners per-peer).
  let testResults = $state<Record<string, { ok: boolean; message: string }>>({});

  // Topology modal state. Single boolean opens a modal that shows the
  // full DAG of every peer's upstream chain converging on this instance.
  let topologyOpen = $state(false);

  async function refreshPeers() {
    peersLoading = true;
    peersError = null;
    try {
      const r = await api.listReplicationPeers(PAGE_SIZE, peersPage * PAGE_SIZE);
      peers = r.items;
      peersTotal = r.total;
    } catch (e) {
      peersError = (e as Error).message;
    } finally {
      peersLoading = false;
    }
  }

  async function refreshRules() {
    rulesLoading = true;
    rulesError = null;
    try {
      const r = await api.listReplicationRules({
        limit: PAGE_SIZE,
        offset: rulesPage * PAGE_SIZE
      });
      rules = r.items;
      rulesTotal = r.total;
    } catch (e) {
      rulesError = (e as Error).message;
    } finally {
      rulesLoading = false;
    }
  }

  async function refreshInbound() {
    inboundLoading = true;
    inboundError = null;
    try {
      const r = await api.listReplicationInbound(PAGE_SIZE, inboundPage * PAGE_SIZE);
      inbound = r.items;
      inboundTotal = r.total;
    } catch (e) {
      inboundError = (e as Error).message;
    } finally {
      inboundLoading = false;
    }
  }

  let lastReplicationVersion = 0;
  $effect(() => {
    // Subscribe to `ChangeKind::Replication` SSE events; reloadKeys is
    // bumped every time the server publishes one. Every bump refetches
    // all three lists.
    const v = reloadKeys.replication;
    if (v !== lastReplicationVersion) {
      lastReplicationVersion = v;
      void refreshPeers();
      void refreshRules();
      void refreshInbound();
    }
  });

  onMount(() => {
    void refreshPeers();
    void refreshRules();
    void refreshInbound();
    tickHandle = setInterval(() => {
      nowSecs = Math.floor(Date.now() / 1000);
    }, 1000);
  });

  onDestroy(() => {
    if (tickHandle) {
      clearInterval(tickHandle);
      tickHandle = null;
    }
  });

  function fmtLag(latest?: number): string {
    if (!latest) return '–';
    const lag = Math.max(0, nowSecs - latest);
    if (lag < 60) return `${lag}s`;
    if (lag < 3600) return `${Math.floor(lag / 60)}m ${lag % 60}s`;
    if (lag < 86400) return `${Math.floor(lag / 3600)}h ${Math.floor((lag % 3600) / 60)}m`;
    return `${Math.floor(lag / 86400)}d`;
  }

  function fmtDuration(secs: number | null | undefined): string {
    if (secs == null) return '–';
    return new Date(secs * 1000).toLocaleString();
  }

  function shortUuid(u: string | null): string {
    return u ? u.slice(0, 8) : '–';
  }

  function nilUuid(u: string | null | undefined): boolean {
    return !u || u === '00000000-0000-0000-0000-000000000000';
  }

  async function submitAddPeer() {
    newPeerError = null;
    newPeerSaving = true;
    try {
      await api.createReplicationPeer({
        name: newPeerName.trim(),
        base_url: newPeerBaseUrl.trim(),
        api_token: newPeerToken.trim(),
        tls_skip_verify: newPeerSkipTls
      });
      showAddPeer = false;
      newPeerName = '';
      newPeerBaseUrl = '';
      newPeerToken = '';
      newPeerSkipTls = false;
      await refreshPeers();
    } catch (e) {
      newPeerError = (e as Error).message;
    } finally {
      newPeerSaving = false;
    }
  }

  function openEditPeer(p: ReplicationPeer) {
    editingPeer = p;
    editPeerName = p.name;
    editPeerToken = '';
    editPeerInterval = p.reconcile_interval_secs;
    editPeerSkipTls = p.tls_skip_verify;
    editPeerError = null;
  }

  async function submitEditPeer() {
    if (!editingPeer) return;
    editPeerError = null;
    editPeerSaving = true;
    try {
      const patch: Record<string, unknown> = {};
      if (editPeerName.trim() && editPeerName.trim() !== editingPeer.name) {
        patch.name = editPeerName.trim();
      }
      if (editPeerToken.trim()) patch.api_token = editPeerToken.trim();
      if (editPeerInterval !== editingPeer.reconcile_interval_secs) {
        patch.reconcile_interval_secs = editPeerInterval;
      }
      if (editPeerSkipTls !== editingPeer.tls_skip_verify) {
        patch.tls_skip_verify = editPeerSkipTls;
      }
      if (Object.keys(patch).length > 0) {
        await api.updateReplicationPeer(editingPeer.uuid, patch);
      }
      editingPeer = null;
      await refreshPeers();
    } catch (e) {
      editPeerError = (e as Error).message;
    } finally {
      editPeerSaving = false;
    }
  }

  async function deletePeer(p: ReplicationPeer) {
    if (
      !confirm(
        `Remove peer "${p.name}"?\n\n` +
          `Replicated hosts and groups stay on disk (with their data), ` +
          `but they detach from the peer and become locally-owned.`
      )
    ) {
      return;
    }
    try {
      await api.deleteReplicationPeer(p.uuid);
      await refreshPeers();
      await refreshRules();
    } catch (e) {
      alert((e as Error).message);
    }
  }

  async function testPeer(p: ReplicationPeer) {
    testResults[p.uuid] = { ok: false, message: 'testing…' };
    try {
      const r = await api.testReplicationPeer(p.uuid);
      testResults[p.uuid] = r.ok
        ? {
            ok: true,
            message: `OK · ${r.source_version ?? '?'} · ${r.latency_ms} ms`
          }
        : { ok: false, message: r.error ?? 'failed' };
    } catch (e) {
      testResults[p.uuid] = { ok: false, message: (e as Error).message };
    }
  }

  async function openAddRule() {
    showAddRule = true;
    newRulePeerUuid = peers[0]?.uuid ?? '';
    newRuleSourceGroup = null;
    newRuleDestGroup = null;
    newRuleError = null;
    newRulePreviewGroups = [];
    if (newRulePeerUuid) {
      await loadPeerGroups(newRulePeerUuid);
    }
    try {
      localGroupsForPicker = await api.listGroups();
    } catch (e) {
      // Non-fatal - the dest-group picker just won't autocomplete.
      console.warn('listGroups failed', e);
    }
  }

  async function loadPeerGroups(peerUuid: string) {
    newRulePreviewGroups = [];
    if (!peerUuid) return;
    try {
      newRulePreviewGroups = await api.replicationPeerGroupsPreview(peerUuid);
    } catch (e) {
      newRuleError = `source groups: ${(e as Error).message}`;
    }
  }

  async function submitAddRule() {
    newRuleError = null;
    newRuleSaving = true;
    try {
      await api.createReplicationRule({
        peer_uuid: newRulePeerUuid,
        source_group_uuid: newRuleSourceGroup,
        dest_group_uuid: newRuleDestGroup,
        enabled: true
      });
      showAddRule = false;
      await refreshRules();
    } catch (e) {
      newRuleError = (e as Error).message;
    } finally {
      newRuleSaving = false;
    }
  }

  async function toggleRule(r: ReplicationRule) {
    try {
      await api.toggleReplicationRule(r.uuid, !r.enabled);
      await refreshRules();
    } catch (e) {
      alert((e as Error).message);
    }
  }

  async function deleteRule(r: ReplicationRule) {
    if (
      !confirm(
        `Remove rule from "${r.peer_name}"?\n\n` +
          `Replicated hosts stay on disk; the source slot will be torn down.`
      )
    ) {
      return;
    }
    try {
      await api.deleteReplicationRule(r.uuid);
      await refreshRules();
    } catch (e) {
      alert((e as Error).message);
    }
  }

  async function deleteInboundSlot(slot: ReplicationInboundSlot) {
    if (
      !confirm(
        `Block inbound slot from "${slot.peer_label}"?\n\n` +
          `The remote destination will immediately start getting 403 on every ` +
          `wire call. Their replication config stays in place on their end - ` +
          `press Unblock here whenever you want to let them through again.`
      )
    ) {
      return;
    }
    try {
      await api.deleteReplicationInbound(slot.slot_uuid);
      await refreshInbound();
    } catch (e) {
      alert((e as Error).message);
    }
  }

  async function unblockInboundSlot(slot: ReplicationInboundSlot) {
    try {
      await api.unblockReplicationInbound(slot.slot_uuid);
      await refreshInbound();
    } catch (e) {
      alert((e as Error).message);
    }
  }

  function groupName(uuid: string): string {
    if (nilUuid(uuid)) return '(root)';
    const local = localGroupsForPicker.find((g) => g.uuid === uuid);
    if (local) return local.display_name;
    const peer = newRulePreviewGroups.find((g) => g.uuid === uuid);
    return peer?.display_name ?? shortUuid(uuid);
  }
</script>

<div class="space-y-2">
  <div class="flex items-baseline gap-3">
    <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
      Replication
    </h2>
    {#if myInstanceUuid}
      <span class="text-[11px]" style="color: var(--muted)">
        My instance id: <code style="font-family: monospace">{myInstanceUuid}</code>
      </span>
    {/if}
  </div>

  <!-- ─── Peers ────────────────────────────────────────────────────────── -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <div class="flex items-center justify-between mb-2">
      <h3 class="text-xs font-semibold">Peers ({peersTotal})</h3>
      <div class="flex items-center gap-2">
        {#if peers.some((p) => p.upstream_chain && p.upstream_chain.length > 0)}
          <button
            type="button"
            class="text-xs rounded px-2 py-1 border"
            style="border-color: var(--border)"
            onclick={() => (topologyOpen = true)}
          >
            Topology
          </button>
        {/if}
        <button
          type="button"
          class="text-xs rounded px-2 py-1"
          style="background: var(--btn-bg); color: var(--btn-text)"
          onclick={() => {
            showAddPeer = true;
            newPeerError = null;
          }}
        >
          + Add peer
        </button>
      </div>
    </div>
    <p class="text-[11px] mb-2" style="color: var(--muted)">
      Each peer is a remote Haze instance this one pulls from. The API token
      must belong to an admin user on the source.
    </p>
    {#if peersLoading}
      <p class="text-xs" style="color: var(--muted)">Loading…</p>
    {:else if peersError}
      <p class="text-xs" style="color: var(--latency-bad)">{peersError}</p>
    {:else if peers.length === 0}
      <p class="text-xs" style="color: var(--muted)">No peers configured.</p>
    {:else}
      <div class="overflow-x-auto">
        <table class="w-full text-xs">
          <thead>
            <tr style="color: var(--muted)">
              <th class="text-left py-1">Name</th>
              <th class="text-left py-1">URL</th>
              <th class="text-left py-1">Source UUID</th>
              <th class="text-left py-1">Last contact</th>
              <th class="text-left py-1">Status</th>
              <th class="text-right py-1">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each peers as p (p.uuid)}
              <tr style="border-top: 1px solid var(--border)">
                <td class="py-1 font-medium">{p.name}</td>
                <td class="py-1" style="color: var(--muted)">{p.base_url}</td>
                <td class="py-1" style="color: var(--muted)">
                  {shortUuid(p.source_instance_uuid)}
                </td>
                <td class="py-1" style="color: var(--muted)">
                  {fmtDuration(p.last_contact_at)}
                </td>
                <td class="py-1">
                  {#if testResults[p.uuid]}
                    <span
                      style="color: {testResults[p.uuid].ok
                        ? 'var(--latency-good)'
                        : 'var(--latency-bad)'}"
                    >
                      {testResults[p.uuid].message}
                    </span>
                  {:else if p.last_error}
                    <span style="color: var(--latency-bad)" title={p.last_error}>error</span>
                  {:else if p.source_version && p.last_latency_ms != null}
                    <span style="color: var(--latency-good)">
                      OK · {p.source_version} · {p.last_latency_ms} ms
                    </span>
                  {:else}
                    <span style="color: var(--latency-good)">ok</span>
                  {/if}
                </td>
                <td class="py-1 text-right">
                  <button class="underline mr-2" onclick={() => testPeer(p)}>Test</button>
                  <button class="underline mr-2" onclick={() => openEditPeer(p)}>Edit</button>
                  <button
                    class="underline"
                    style="color: var(--latency-bad)"
                    onclick={() => deletePeer(p)}
                  >
                    Delete
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {@render Pagination({
        page: peersPage,
        total: peersTotal,
        pageSize: PAGE_SIZE,
        onChange: (p: number) => {
          peersPage = p;
          void refreshPeers();
        }
      })}
    {/if}
  </section>

  <!-- ─── Rules ─────────────────────────────────────────────────────────── -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <div class="flex items-center justify-between mb-2">
      <h3 class="text-xs font-semibold">Rules ({rulesTotal})</h3>
      <button
        type="button"
        class="text-xs rounded px-2 py-1"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {peers.length === 0
          ? 0.5
          : 1}"
        disabled={peers.length === 0}
        onclick={() => openAddRule()}
      >
        + Add rule
      </button>
    </div>
    <p class="text-[11px] mb-2" style="color: var(--muted)">
      Each rule maps a source group (or root) to a local destination group
      (or root). Same-named groups under the same parent merge automatically.
      The "Lag" column ticks live; updates every ~30 s as samples are
      ingested.
    </p>
    {#if rulesLoading}
      <p class="text-xs" style="color: var(--muted)">Loading…</p>
    {:else if rulesError}
      <p class="text-xs" style="color: var(--latency-bad)">{rulesError}</p>
    {:else if rules.length === 0}
      <p class="text-xs" style="color: var(--muted)">No rules configured.</p>
    {:else}
      <div class="overflow-x-auto">
        <table class="w-full text-xs">
          <thead>
            <tr style="color: var(--muted)">
              <th class="text-left py-1">Peer</th>
              <th class="text-left py-1">Source → Dest</th>
              <th class="text-left py-1">Hosts</th>
              <th class="text-left py-1">Lag</th>
              <th class="text-left py-1">State</th>
              <th class="text-right py-1">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each rules as r (r.uuid)}
              <tr style="border-top: 1px solid var(--border)">
                <td class="py-1 font-medium">{r.peer_name}</td>
                <td class="py-1" style="color: var(--muted)">
                  {nilUuid(r.source_group_uuid)
                    ? '(root)'
                    : shortUuid(r.source_group_uuid)} →
                  {nilUuid(r.dest_group_uuid)
                    ? '(root)'
                    : shortUuid(r.dest_group_uuid)}
                </td>
                <td class="py-1">{r.host_count}</td>
                <td
                  class="py-1"
                  title={r.latest_ingested_ts
                    ? `last sample at ${fmtDuration(r.latest_ingested_ts)}`
                    : 'no samples yet'}
                  style="font-variant-numeric: tabular-nums"
                >
                  {fmtLag(r.latest_ingested_ts)}
                </td>
                <td class="py-1">
                  {#if !r.enabled}
                    <span style="color: var(--muted)">paused</span>
                  {:else if r.last_error}
                    <span style="color: var(--latency-bad)" title={r.last_error}>error</span>
                  {:else}
                    <span style="color: var(--latency-good)">active</span>
                  {/if}
                </td>
                <td class="py-1 text-right">
                  <button class="underline mr-2" onclick={() => toggleRule(r)}>
                    {r.enabled ? 'Pause' : 'Resume'}
                  </button>
                  <button
                    class="underline"
                    style="color: var(--latency-bad)"
                    onclick={() => deleteRule(r)}
                  >
                    Delete
                  </button>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {@render Pagination({
        page: rulesPage,
        total: rulesTotal,
        pageSize: PAGE_SIZE,
        onChange: (p: number) => {
          rulesPage = p;
          void refreshRules();
        }
      })}
    {/if}
  </section>

  <!-- ─── Inbound ───────────────────────────────────────────────────────── -->
  <section class="border rounded p-3" style="border-color: var(--border)">
    <h3 class="text-xs font-semibold mb-2">Inbound slots ({inboundTotal})</h3>
    <p class="text-[11px] mb-2" style="color: var(--muted)">
      Other Haze instances that are pulling from this one. Press "Block"
      to immediately cut a destination off - it will start seeing 403 on
      every wire call until you press "Unblock" here. Their configuration
      stays in place, so unblocking is enough to let replication resume.
    </p>
    {#if inboundLoading}
      <p class="text-xs" style="color: var(--muted)">Loading…</p>
    {:else if inboundError}
      <p class="text-xs" style="color: var(--latency-bad)">{inboundError}</p>
    {:else if inbound.length === 0}
      <p class="text-xs" style="color: var(--muted)">No inbound replication.</p>
    {:else}
      <div class="overflow-x-auto">
        <table class="w-full text-xs">
          <thead>
            <tr style="color: var(--muted)">
              <th class="text-left py-1">Label</th>
              <th class="text-left py-1">Group</th>
              <th class="text-left py-1">Hops</th>
              <th class="text-left py-1">Hosts</th>
              <th class="text-left py-1">Last stream</th>
              <th class="text-right py-1">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each inbound as s (s.slot_uuid)}
              <tr style="border-top: 1px solid var(--border)">
                <td class="py-1 font-medium">{s.peer_label}</td>
                <td class="py-1" style="color: var(--muted)">
                  {nilUuid(s.source_group_uuid) ? '(root)' : shortUuid(s.source_group_uuid)}
                </td>
                <td
                  class="py-1"
                  style="color: var(--muted)"
                  title={s.replication_path.join(' → ')}
                >
                  {s.replication_path.length}
                </td>
                <td class="py-1">{s.host_count}</td>
                <td class="py-1" style="color: var(--muted)">
                  {#if s.blocked_at}
                    <span style="color: var(--latency-bad)">
                      Blocked {fmtDuration(s.blocked_at)}
                    </span>
                  {:else}
                    {fmtDuration(s.last_stream_at)}
                  {/if}
                </td>
                <td class="py-1 text-right">
                  {#if s.blocked_at}
                    <button
                      class="underline"
                      style="color: var(--latency-good)"
                      onclick={() => unblockInboundSlot(s)}
                    >
                      Unblock
                    </button>
                  {:else}
                    <button
                      class="underline"
                      style="color: var(--latency-bad)"
                      onclick={() => deleteInboundSlot(s)}
                    >
                      Block
                    </button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
      {@render Pagination({
        page: inboundPage,
        total: inboundTotal,
        pageSize: PAGE_SIZE,
        onChange: (p: number) => {
          inboundPage = p;
          void refreshInbound();
        }
      })}
    {/if}
  </section>
</div>

{#if topologyOpen}
  <ReplicationTopology {peers} onClose={() => (topologyOpen = false)} />
{/if}

<!-- ─── Add-peer modal ────────────────────────────────────────────────── -->
{#if showAddPeer}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center"
    style="background: rgba(0,0,0,0.4)"
  >
    <div
      class="rounded border p-4 w-[28rem]"
      style="background: var(--bg); border-color: var(--border); color: var(--fg)"
    >
      <h3 class="text-sm font-semibold mb-3">Add replication peer</h3>
      <label class="block text-xs mb-2">
        Name
        <input
          type="text"
          bind:value={newPeerName}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block text-xs mb-2">
        Base URL
        <input
          type="text"
          placeholder="https://haze.example.com"
          bind:value={newPeerBaseUrl}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block text-xs mb-2">
        Admin API token (hzt_...)
        <input
          type="password"
          bind:value={newPeerToken}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="flex items-center gap-2 text-xs mb-3">
        <input type="checkbox" bind:checked={newPeerSkipTls} />
        Skip TLS verification (self-signed source)
      </label>
      {#if newPeerError}
        <p class="text-xs mb-2" style="color: var(--latency-bad)">{newPeerError}</p>
      {/if}
      <div class="flex justify-end gap-2 text-xs">
        <button
          type="button"
          class="rounded px-2 py-1 border"
          style="border-color: var(--border)"
          onclick={() => (showAddPeer = false)}
        >
          Cancel
        </button>
        <button
          type="button"
          class="rounded px-2 py-1"
          style="background: var(--btn-bg); color: var(--btn-text); opacity: {newPeerSaving
            ? 0.6
            : 1}"
          disabled={newPeerSaving}
          onclick={submitAddPeer}
        >
          {newPeerSaving ? 'Pairing…' : 'Add peer'}
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- ─── Edit-peer modal ───────────────────────────────────────────────── -->
{#if editingPeer}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center"
    style="background: rgba(0,0,0,0.4)"
  >
    <div
      class="rounded border p-4 w-[28rem]"
      style="background: var(--bg); border-color: var(--border); color: var(--fg)"
    >
      <h3 class="text-sm font-semibold mb-3">Edit peer: {editingPeer.name}</h3>
      <label class="block text-xs mb-2">
        Name
        <input
          type="text"
          bind:value={editPeerName}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block text-xs mb-2">
        New API token (leave empty to keep current)
        <input
          type="password"
          bind:value={editPeerToken}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="block text-xs mb-2">
        Reconcile interval (seconds, 30-86400)
        <input
          type="number"
          min="30"
          max="86400"
          bind:value={editPeerInterval}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        />
      </label>
      <label class="flex items-center gap-2 text-xs mb-3">
        <input type="checkbox" bind:checked={editPeerSkipTls} />
        Skip TLS verification
      </label>
      {#if editPeerError}
        <p class="text-xs mb-2" style="color: var(--latency-bad)">{editPeerError}</p>
      {/if}
      <div class="flex justify-end gap-2 text-xs">
        <button
          type="button"
          class="rounded px-2 py-1 border"
          style="border-color: var(--border)"
          onclick={() => (editingPeer = null)}
        >
          Cancel
        </button>
        <button
          type="button"
          class="rounded px-2 py-1"
          style="background: var(--btn-bg); color: var(--btn-text); opacity: {editPeerSaving
            ? 0.6
            : 1}"
          disabled={editPeerSaving}
          onclick={submitEditPeer}
        >
          {editPeerSaving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </div>
  </div>
{/if}

<!-- ─── Add-rule modal ────────────────────────────────────────────────── -->
{#if showAddRule}
  <div
    class="fixed inset-0 z-50 flex items-center justify-center"
    style="background: rgba(0,0,0,0.4)"
  >
    <div
      class="rounded border p-4 w-[32rem]"
      style="background: var(--bg); border-color: var(--border); color: var(--fg)"
    >
      <h3 class="text-sm font-semibold mb-3">Add replication rule</h3>
      <label class="block text-xs mb-2">
        Peer
        <select
          bind:value={newRulePeerUuid}
          onchange={() => loadPeerGroups(newRulePeerUuid)}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        >
          {#each peers as p (p.uuid)}
            <option value={p.uuid}>{p.name}</option>
          {/each}
        </select>
      </label>
      <label class="block text-xs mb-2">
        Source group (on peer)
        <select
          bind:value={newRuleSourceGroup}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        >
          <option value={null}>(root)</option>
          {#each newRulePreviewGroups as g (g.uuid)}
            <option value={g.uuid}>{'  '.repeat(g.depth)}{g.display_name}</option>
          {/each}
        </select>
      </label>
      <label class="block text-xs mb-3">
        Destination group (local)
        <select
          bind:value={newRuleDestGroup}
          class="block w-full mt-1 border rounded px-2 py-1"
          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
        >
          <option value={null}>(root)</option>
          {#each localGroupsForPicker as g (g.uuid)}
            <option value={g.uuid}>{'  '.repeat(g.depth)}{g.display_name}</option>
          {/each}
        </select>
      </label>
      {#if newRuleError}
        <p class="text-xs mb-2" style="color: var(--latency-bad)">{newRuleError}</p>
      {/if}
      <div class="flex justify-end gap-2 text-xs">
        <button
          type="button"
          class="rounded px-2 py-1 border"
          style="border-color: var(--border)"
          onclick={() => (showAddRule = false)}
        >
          Cancel
        </button>
        <button
          type="button"
          class="rounded px-2 py-1"
          style="background: var(--btn-bg); color: var(--btn-text); opacity: {newRuleSaving
            ? 0.6
            : 1}"
          disabled={newRuleSaving || !newRulePeerUuid}
          onclick={submitAddRule}
        >
          {newRuleSaving ? 'Saving…' : 'Add rule'}
        </button>
      </div>
    </div>
  </div>
{/if}

{#snippet Pagination(props: {
  page: number;
  total: number;
  pageSize: number;
  onChange: (p: number) => void;
})}
  {@const pages = Math.max(1, Math.ceil(props.total / props.pageSize))}
  <div
    class="flex items-center justify-between mt-2 text-[11px]"
    style="color: var(--muted)"
  >
    <span>
      {props.total === 0
        ? '0 items'
        : `${props.page * props.pageSize + 1}-${Math.min(
            (props.page + 1) * props.pageSize,
            props.total
          )} of ${props.total}`}
    </span>
    <span class="flex items-center gap-2">
      <button
        class="border rounded px-1.5 py-0.5"
        style="border-color: var(--border); opacity: {props.page === 0 ? 0.4 : 1}"
        disabled={props.page === 0}
        onclick={() => props.onChange(props.page - 1)}
      >
        Prev
      </button>
      <span>Page {props.page + 1} / {pages}</span>
      <button
        class="border rounded px-1.5 py-0.5"
        style="border-color: var(--border); opacity: {props.page + 1 >= pages ? 0.4 : 1}"
        disabled={props.page + 1 >= pages}
        onclick={() => props.onChange(props.page + 1)}
      >
        Next
      </button>
    </span>
  </div>
{/snippet}
