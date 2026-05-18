<script lang="ts">
  import { canSeeAlerts, canEditAlerts } from '$lib/auth.svelte';
  import {
    api,
    type AlertRule,
    type AlertState,
    type Group,
    type Host,
    type Severity,
    type Webhook,
    ApiError
  } from '$lib/api';
  import { reloadKeys } from '$lib/events.svelte';
  import AlertRuleModal from '$lib/components/AlertRuleModal.svelte';
  import Forbidden from '$lib/components/Forbidden.svelte';
  import { onMount, onDestroy } from 'svelte';

  let rules = $state<AlertRule[]>([]);
  let states = $state<AlertState[]>([]);
  let webhooks = $state<Webhook[]>([]);
  let groups = $state<Group[]>([]);
  let hosts = $state<Host[]>([]);

  let loading = $state(true);
  let err = $state<string | null>(null);

  let modalOpen = $state(false);
  let editTarget = $state<AlertRule | null>(null);

  // Per-row feedback, keyed by rule uuid so each row keeps its own message.
  let rowErr = $state<Record<string, string | null>>({});
  let rowBusy = $state<Record<string, boolean>>({});

  async function refresh() {
    loading = true;
    err = null;
    try {
      const [r, s, g, h] = await Promise.all([
        api.listAlertRules(),
        api.listAlertStates(),
        api.listGroups(),
        api.listHosts()
      ]);
      rules = r;
      states = s;
      groups = g;
      hosts = h;
      // Webhooks are admin-only; for users we list rules but skip webhook
      // names. Don't surface the 403 as a page-level error.
      try {
        webhooks = await api.listWebhooks();
      } catch (e) {
        if (!(e instanceof ApiError && e.status === 403)) throw e;
        webhooks = [];
      }
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  // Poll alert states so "Currently firing" and the per-row severity dots
  // pick up backend evaluator changes without the user reloading the page.
  // States are the volatile bit; rules/groups/hosts/webhooks are refetched
  // only by explicit user actions (edit/delete/save).
  let pollTimer: ReturnType<typeof setInterval> | undefined;

  async function pollStates() {
    if (!canSeeAlerts()) return;
    try {
      states = await api.listAlertStates();
    } catch {
      // Swallow transient errors: the next tick will retry. A page-level
      // error banner here would flicker on every blip.
    }
  }

  onMount(async () => {
    if (!canSeeAlerts()) return;
    await refresh();
    pollTimer = setInterval(() => void pollStates(), 5_000);
  });

  onDestroy(() => {
    if (pollTimer) clearInterval(pollTimer);
  });

  // SSE-driven refresh. Rules and webhook names come from full `refresh()`;
  // tree changes (groups/hosts) only affect label rendering for the rule
  // target column, so the same path is fine. Alert *states* keep their
  // 5 s poll — those flip on backend evaluation cycles, not on a user
  // mutation, and the SSE channel doesn't carry state-transition events.
  $effect(() => {
    void reloadKeys.alerts;
    void reloadKeys.webhooks;
    void reloadKeys.tree;
    if (canSeeAlerts()) void refresh();
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

  function targetLabel(t: { kind: 'host' | 'group'; uuid: string }): string {
    if (t.kind === 'host') {
      const h = hosts.find((x) => x.uuid === t.uuid);
      return h ? `[H] ${h.display_name}` : `[H] ${t.uuid.slice(0, 8)}…`;
    }
    const g = groups.find((x) => x.uuid === t.uuid);
    return g ? `[G] ${breadcrumb(g)}` : `[G] ${t.uuid.slice(0, 8)}…`;
  }

  function summariseTargets(targets: { kind: 'host' | 'group'; uuid: string }[]): string[] {
    return targets.map(targetLabel);
  }

  function thresholdSummary(rule: AlertRule): string {
    const parts: string[] = [];
    if (rule.warning_threshold != null) parts.push(`warn ${rule.warning_threshold}`);
    if (rule.critical_threshold != null) parts.push(`crit ${rule.critical_threshold}`);
    const direction = rule.direction === 'above' ? '≥' : '≤';
    return `${parts.join(' / ')} (${direction})`;
  }

  function metricLabel(m: AlertRule['metric']): string {
    switch (m) {
      case 'min':
        return 'min';
      case 'p2_5':
        return 'p2.5';
      case 'p25':
        return 'p25';
      case 'median':
        return 'median';
      case 'p75':
        return 'p75';
      case 'p97_5':
        return 'p97.5';
      case 'loss_pct':
        return 'loss%';
    }
  }

  function formatWindow(secs: number): string {
    if (secs % 3600 === 0) return `${secs / 3600}h`;
    if (secs % 60 === 0) return `${secs / 60}m`;
    return `${secs}s`;
  }

  // Short numeric rendering for the firing table: integers stay integers,
  // floats get up to 3 significant digits so "312.4" doesn't render as
  // "312.40000000000001" after the f32 → f64 widening in transit.
  function formatValue(v: number): string {
    if (!Number.isFinite(v)) return '—';
    if (Number.isInteger(v)) return v.toString();
    return v.toFixed(Math.abs(v) >= 100 ? 1 : Math.abs(v) >= 10 ? 2 : 3);
  }

  /// Worst severity across every (rule, host) state for this rule.
  function worstSeverityFor(ruleUuid: string): Severity {
    let worst: Severity = 'ok';
    for (const s of states) {
      if (s.rule_uuid !== ruleUuid) continue;
      if (s.severity === 'critical') return 'critical';
      if (s.severity === 'warning') worst = 'warning';
    }
    return worst;
  }

  function severityColor(s: Severity): string {
    switch (s) {
      case 'critical':
        return 'var(--latency-bad)';
      case 'warning':
        return 'var(--latency-warn)';
      case 'ok':
        return 'var(--latency-good)';
    }
  }

  function webhookSummary(rule: AlertRule): string {
    if (rule.webhook_uuids.length === 0) return '—';
    if (webhooks.length === 0) return `${rule.webhook_uuids.length}`;
    const names = rule.webhook_uuids
      .map((u) => webhooks.find((w) => w.uuid === u)?.name ?? u.slice(0, 6))
      .join(', ');
    return names;
  }

  async function deleteRule(rule: AlertRule) {
    if (!window.confirm(`Delete alert "${rule.name}"?`)) return;
    rowBusy = { ...rowBusy, [rule.uuid]: true };
    rowErr = { ...rowErr, [rule.uuid]: null };
    try {
      await api.deleteAlertRule(rule.uuid);
      await refresh();
    } catch (e) {
      rowErr = {
        ...rowErr,
        [rule.uuid]: e instanceof Error ? e.message : String(e)
      };
    } finally {
      rowBusy = { ...rowBusy, [rule.uuid]: false };
    }
  }

  async function toggleEnabled(rule: AlertRule) {
    rowBusy = { ...rowBusy, [rule.uuid]: true };
    rowErr = { ...rowErr, [rule.uuid]: null };
    try {
      await api.updateAlertRule(rule.uuid, {
        name: rule.name,
        enabled: !rule.enabled,
        metric: rule.metric,
        aggregation: rule.aggregation,
        direction: rule.direction,
        warning_threshold: rule.warning_threshold,
        critical_threshold: rule.critical_threshold,
        window_secs: rule.window_secs,
        targets: rule.targets,
        webhook_uuids: rule.webhook_uuids
      });
      await refresh();
    } catch (e) {
      rowErr = {
        ...rowErr,
        [rule.uuid]: e instanceof Error ? e.message : String(e)
      };
    } finally {
      rowBusy = { ...rowBusy, [rule.uuid]: false };
    }
  }
</script>

{#if !canSeeAlerts()}
  <Forbidden what="alerting" />
{:else}
  <div class="p-3 md:p-6 w-full space-y-4">
    <div class="flex items-center justify-between">
      <h1 class="text-base font-semibold">Alerting</h1>
      {#if canEditAlerts()}
        <button
          type="button"
          onclick={() => {
            editTarget = null;
            modalOpen = true;
          }}
          class="px-3 py-1 rounded font-medium text-xs"
          style="background: var(--btn-bg); color: var(--btn-text)"
        >
          + New alert
        </button>
      {/if}
    </div>

    {#if loading}
      <p class="text-xs" style="color: var(--muted)">Loading…</p>
    {:else if err}
      <p class="text-xs" style="color: var(--latency-bad)">{err}</p>
    {:else if rules.length === 0}
      <section class="border rounded p-4 text-xs" style="border-color: var(--border)">
        <p style="color: var(--muted)">
          No alert rules defined yet.
          {#if canEditAlerts()}
            Click <strong>+ New alert</strong> to create one. Rules can target
            any mix of hosts and groups; you'll be asked for a metric, an
            aggregation, a sliding window, and one or two thresholds.
          {/if}
        </p>
      </section>
    {:else}
      <section class="border rounded overflow-hidden" style="border-color: var(--border)">
        <div class="overflow-x-auto">
        <table class="w-full text-xs mono">
          <thead style="color: var(--muted)">
            <tr class="text-left">
              <th class="py-1 px-2 font-normal w-8"></th>
              <th class="py-1 px-2 font-normal">Name</th>
              <th class="py-1 px-2 font-normal">Targets</th>
              <th class="py-1 px-2 font-normal">Metric</th>
              <th class="py-1 px-2 font-normal">Window</th>
              <th class="py-1 px-2 font-normal">Thresholds</th>
              <th class="py-1 px-2 font-normal">Webhooks</th>
              <th class="py-1 px-2 font-normal text-right">Actions</th>
            </tr>
          </thead>
          <tbody>
            {#each rules as rule (rule.uuid)}
              {@const sev = worstSeverityFor(rule.uuid)}
              <tr class="border-t align-top" style="border-color: var(--border)">
                <td class="py-1 px-2">
                  <span
                    class="inline-block w-2 h-2 rounded-full"
                    style="background: {severityColor(sev)}"
                    title={sev}
                  ></span>
                </td>
                <td class="py-1 px-2">
                  <div class="flex items-center gap-1">
                    <span style="font-weight: {rule.enabled ? 600 : 400}; opacity: {rule.enabled ? 1 : 0.5}">
                      {rule.name}
                    </span>
                    {#if !rule.enabled}
                      <span class="text-[10px] uppercase" style="color: var(--muted)">disabled</span>
                    {/if}
                  </div>
                </td>
                <td class="py-1 px-2">
                  <div class="flex flex-wrap gap-1">
                    {#each summariseTargets(rule.targets).slice(0, 3) as label (label)}
                      <span
                        class="inline-block px-1 py-0.5 rounded text-[10px]"
                        style="background: var(--border)"
                      >
                        {label}
                      </span>
                    {/each}
                    {#if rule.targets.length > 3}
                      <span class="text-[10px]" style="color: var(--muted)">
                        +{rule.targets.length - 3} more
                      </span>
                    {/if}
                  </div>
                </td>
                <td class="py-1 px-2">{rule.aggregation}({metricLabel(rule.metric)})</td>
                <td class="py-1 px-2">{formatWindow(rule.window_secs)}</td>
                <td class="py-1 px-2">{thresholdSummary(rule)}</td>
                <td class="py-1 px-2 truncate" style="max-width: 220px" title={webhookSummary(rule)}>
                  {webhookSummary(rule)}
                </td>
                <td class="py-1 px-2 text-right whitespace-nowrap">
                  {#if canEditAlerts()}
                    <button
                      type="button"
                      onclick={() => toggleEnabled(rule)}
                      disabled={rowBusy[rule.uuid]}
                      class="px-1 py-0.5"
                      style="color: var(--fg)"
                    >
                      {rule.enabled ? 'disable' : 'enable'}
                    </button>
                    <button
                      type="button"
                      onclick={() => {
                        editTarget = rule;
                        modalOpen = true;
                      }}
                      disabled={rowBusy[rule.uuid]}
                      class="px-1 py-0.5 ml-1"
                      style="color: var(--fg)"
                    >
                      edit
                    </button>
                    <button
                      type="button"
                      onclick={() => deleteRule(rule)}
                      disabled={rowBusy[rule.uuid]}
                      class="px-1 py-0.5 ml-1"
                      style="color: var(--latency-bad)"
                    >
                      delete
                    </button>
                  {/if}
                </td>
              </tr>
              {#if rowErr[rule.uuid]}
                <tr>
                  <td colspan="8" class="px-2 pb-1">
                    <p class="text-[11px]" style="color: var(--latency-bad)">
                      {rowErr[rule.uuid]}
                    </p>
                  </td>
                </tr>
              {/if}
            {/each}
          </tbody>
        </table>
        </div>
      </section>

      {#if states.some((s) => s.severity !== 'ok')}
        <section class="border rounded p-3" style="border-color: var(--border)">
          <h2 class="text-xs font-semibold mb-2" style="color: var(--muted)">
            Currently firing
          </h2>
          <div class="overflow-x-auto -mx-3 px-3">
          <table class="w-full text-xs mono">
            <thead style="color: var(--muted)">
              <tr class="text-left">
                <th class="py-1 px-2 font-normal">Severity</th>
                <th class="py-1 px-2 font-normal">Rule</th>
                <th class="py-1 px-2 font-normal">Host</th>
                <th class="py-1 px-2 font-normal">Comparison</th>
                <th class="py-1 px-2 font-normal">Since</th>
              </tr>
            </thead>
            <tbody>
              {#each states.filter((s) => s.severity !== 'ok') as s (`${s.rule_uuid}-${s.host_uuid}`)}
                {@const rule = rules.find((r) => r.uuid === s.rule_uuid)}
                {@const host = hosts.find((h) => h.uuid === s.host_uuid)}
                <tr class="border-t" style="border-color: var(--border)">
                  <td class="py-1 px-2" style="color: {severityColor(s.severity)}">
                    {s.severity}
                  </td>
                  <td class="py-1 px-2">{rule?.name ?? s.rule_uuid.slice(0, 8)}</td>
                  <td class="py-1 px-2">{host?.display_name ?? s.host_uuid.slice(0, 8)}</td>
                  <td class="py-1 px-2">
                    {#if rule && s.last_value != null && s.last_threshold != null}
                      <span>
                        {rule.aggregation}({metricLabel(rule.metric)}) = {formatValue(s.last_value)}
                        <span style="color: var(--muted)">
                          {rule.direction === 'above' ? '≥' : '≤'}
                        </span>
                        {formatValue(s.last_threshold)}
                      </span>
                    {:else if s.last_value != null}
                      <span>{formatValue(s.last_value)}</span>
                    {:else}
                      <span style="color: var(--muted)">—</span>
                    {/if}
                  </td>
                  <td class="py-1 px-2" style="color: var(--muted)">
                    {new Date(s.since * 1000).toLocaleString()}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
          </div>
        </section>
      {/if}
    {/if}
  </div>

  {#if modalOpen}
    <!-- {#key} forces a fresh modal mount when the user clicks "edit" on a
         different rule, so the form's $state initialisers re-fire with
         the new defaults. -->
    {#key editTarget?.uuid ?? 'new'}
      <AlertRuleModal
        initial={editTarget ?? undefined}
        onClose={() => {
          modalOpen = false;
          editTarget = null;
        }}
        onSaved={() => void refresh()}
      />
    {/key}
  {/if}
{/if}
