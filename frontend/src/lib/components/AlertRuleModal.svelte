<script lang="ts">
  import {
    api,
    ApiError,
    type AlertAggregation,
    type AlertDirection,
    type AlertMetric,
    type AlertRule,
    type AlertRuleInput,
    type AlertTarget,
    type Group,
    type Host,
    type Webhook
  } from '$lib/api';
  import Modal from './Modal.svelte';

  // Single component used for both create and edit: pass `initial` to
  // pre-populate, leave it undefined to start blank.
  let {
    initial,
    onClose,
    onSaved
  }: {
    initial?: AlertRule;
    onClose: () => void;
    onSaved: () => void;
  } = $props();

  const METRIC_LABELS: Record<AlertMetric, string> = {
    min: 'min latency (ms)',
    p2_5: 'p2.5 latency (ms)',
    p25: 'p25 latency (ms)',
    median: 'median latency (ms)',
    p75: 'p75 latency (ms)',
    p97_5: 'p97.5 latency (ms)',
    loss_pct: 'loss %'
  };
  const METRIC_ORDER: AlertMetric[] = [
    'median',
    'p75',
    'p97_5',
    'p25',
    'p2_5',
    'min',
    'loss_pct'
  ];

  const AGG_LABELS: Record<AlertAggregation, string> = {
    max: 'max',
    avg: 'avg',
    min: 'min',
    p50: 'median (p50)',
    p75: 'p75',
    p90: 'p90',
    p95: 'p95',
    p99: 'p99'
  };
  const AGG_ORDER: AlertAggregation[] = ['avg', 'max', 'min', 'p50', 'p75', 'p90', 'p95', 'p99'];

  // svelte-ignore state_referenced_locally
  let name = $state(initial?.name ?? '');
  // svelte-ignore state_referenced_locally
  let enabled = $state(initial?.enabled ?? true);
  // svelte-ignore state_referenced_locally
  let metric = $state<AlertMetric>(initial?.metric ?? 'median');
  // svelte-ignore state_referenced_locally
  let aggregation = $state<AlertAggregation>(initial?.aggregation ?? 'avg');
  // svelte-ignore state_referenced_locally
  let direction = $state<AlertDirection>(initial?.direction ?? 'above');
  // svelte-ignore state_referenced_locally
  let windowSecs = $state(initial?.window_secs ?? 300);
  // svelte-ignore state_referenced_locally
  let warningEnabled = $state(initial?.warning_threshold != null);
  // svelte-ignore state_referenced_locally
  let warningValue = $state<number>(initial?.warning_threshold ?? 100);
  // svelte-ignore state_referenced_locally
  let criticalEnabled = $state(initial?.critical_threshold != null);
  // svelte-ignore state_referenced_locally
  let criticalValue = $state<number>(initial?.critical_threshold ?? 250);

  // Targets are stored as typed entries; the picker offers both groups and
  // hosts (prefixed [G] / [H]) in one combobox so users can mix freely.
  // svelte-ignore state_referenced_locally
  let targets = $state<AlertTarget[]>(initial?.targets ? [...initial.targets] : []);
  // svelte-ignore state_referenced_locally
  let webhookUuids = $state<string[]>(
    initial?.webhook_uuids ? [...initial.webhook_uuids] : []
  );

  let groups = $state<Group[]>([]);
  let hosts = $state<Host[]>([]);
  let webhooks = $state<Webhook[]>([]);
  let webhooksAccessible = $state(true);

  let targetSearch = $state('');
  let targetPickerOpen = $state(false);
  let targetInputEl: HTMLInputElement | undefined = $state();
  let targetListEl: HTMLDivElement | undefined = $state();
  let targetHighlighted = $state(0);

  let busy = $state(false);
  let err = $state<string | null>(null);

  $effect(() => {
    void Promise.all([api.listGroups(), api.listHosts()]).then(([g, h]) => {
      groups = g;
      hosts = h;
    });
  });

  $effect(() => {
    // Webhooks are admin-only; for non-admins this returns 403. Treat
    // that as "no webhooks available" instead of bubbling an error.
    void api.listWebhooks().then(
      (w) => {
        webhooks = w;
        webhooksAccessible = true;
      },
      (e) => {
        if (e instanceof ApiError && e.status === 403) {
          webhooksAccessible = false;
        }
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

  type TargetSuggestion =
    | { kind: 'host'; uuid: string; label: string }
    | { kind: 'group'; uuid: string; label: string };

  const MAX_SUGGESTIONS = 15;

  // Simple fuzzy: every needle char must appear in haystack in order.
  // Lower score = better (rewards contiguous matches + early position).
  // Returns null when there is no match.
  function fuzzyScore(haystack: string, needle: string): number | null {
    if (!needle) return 0;
    let h = 0;
    let score = 0;
    let lastMatch = -1;
    for (let n = 0; n < needle.length; n++) {
      const c = needle[n];
      let found = -1;
      while (h < haystack.length) {
        if (haystack[h] === c) {
          found = h;
          h++;
          break;
        }
        h++;
      }
      if (found === -1) return null;
      if (lastMatch !== -1) {
        // Penalise gaps between consecutive matched chars.
        score += found - lastMatch - 1;
      } else {
        // Small penalty for matches that start late in the string,
        // so "prod-us" beats "us-prod" for query "prod".
        score += found;
      }
      lastMatch = found;
    }
    return score;
  }

  // Groups come first — alerting on a group is the common case (one rule
  // covers everything underneath it), so putting them at the top makes
  // the keyboard-only flow (focus, type, Enter) pick a group by default.
  const targetSuggestions = $derived.by<TargetSuggestion[]>(() => {
    const q = targetSearch.trim().toLowerCase();
    type Scored = { item: TargetSuggestion; score: number; order: number };
    const groupHits: Scored[] = [];
    let order = 0;
    for (const g of groups) {
      if (targets.some((t) => t.kind === 'group' && t.uuid === g.uuid)) continue;
      const label = breadcrumb(g);
      const score = fuzzyScore(label.toLowerCase(), q);
      if (score == null) continue;
      groupHits.push({ item: { kind: 'group', uuid: g.uuid, label }, score, order: order++ });
    }
    const hostHits: Scored[] = [];
    for (const h of hosts) {
      if (targets.some((t) => t.kind === 'host' && t.uuid === h.uuid)) continue;
      const score = fuzzyScore(h.display_name.toLowerCase(), q);
      if (score == null) continue;
      hostHits.push({
        item: { kind: 'host', uuid: h.uuid, label: h.display_name },
        score,
        order: order++
      });
    }
    // When there's a query, sort by fuzzy score across BOTH kinds so the
    // best match wins regardless of host/group. With no query, preserve
    // original ordering (groups first, then hosts).
    if (q) {
      const all = [...groupHits, ...hostHits];
      all.sort((a, b) => a.score - b.score || a.order - b.order);
      return all.slice(0, MAX_SUGGESTIONS).map((s) => s.item);
    }
    return [...groupHits, ...hostHits].slice(0, MAX_SUGGESTIONS).map((s) => s.item);
  });


  function addTarget(s: TargetSuggestion) {
    targets = [...targets, { kind: s.kind, uuid: s.uuid }];
    targetSearch = '';
    // Keep the picker open so the user can keep typing and adding more.
    // We can't just call focus() to re-trigger onfocus — the input is
    // already focused (the suggestion button's onmousedown preventDefault
    // keeps it that way), so onfocus wouldn't fire.
    targetPickerOpen = true;
    targetInputEl?.focus();
  }

  function removeTarget(index: number) {
    targets = targets.filter((_, i) => i !== index);
  }

  $effect(() => {
    void targetSuggestions.length;
    targetHighlighted = 0;
  });
  $effect(() => {
    if (!targetPickerOpen) return;
    const i = targetHighlighted;
    queueMicrotask(() => {
      const el = targetListEl?.children[i] as HTMLElement | undefined;
      el?.scrollIntoView({ block: 'nearest' });
    });
  });

  function onTargetInputKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && targetPickerOpen) {
      targetPickerOpen = false;
      e.stopPropagation();
      e.preventDefault();
      return;
    }
    if (e.key === 'Backspace' && targetSearch === '' && targets.length > 0) {
      targets = targets.slice(0, -1);
    } else if (e.key === 'ArrowDown' && targetSuggestions.length > 0) {
      e.preventDefault();
      targetHighlighted = (targetHighlighted + 1) % targetSuggestions.length;
    } else if (e.key === 'ArrowUp' && targetSuggestions.length > 0) {
      e.preventDefault();
      targetHighlighted =
        (targetHighlighted - 1 + targetSuggestions.length) % targetSuggestions.length;
    } else if (e.key === 'Enter' && targetSuggestions.length > 0) {
      e.preventDefault();
      const idx = Math.min(targetHighlighted, targetSuggestions.length - 1);
      addTarget(targetSuggestions[idx]);
    }
  }

  function toggleWebhook(uuid: string) {
    if (webhookUuids.includes(uuid)) {
      webhookUuids = webhookUuids.filter((u) => u !== uuid);
    } else {
      webhookUuids = [...webhookUuids, uuid];
    }
  }

  function targetLabel(t: AlertTarget): string {
    if (t.kind === 'host') {
      const h = hosts.find((x) => x.uuid === t.uuid);
      return h ? `[H] ${h.display_name}` : `[H] ${t.uuid.slice(0, 8)}…`;
    }
    const g = groups.find((x) => x.uuid === t.uuid);
    return g ? `[G] ${breadcrumb(g)}` : `[G] ${t.uuid.slice(0, 8)}…`;
  }

  const validationError = $derived.by(() => {
    if (!name.trim()) return 'Name is required.';
    if (targets.length === 0) return 'Pick at least one host or group as a target.';
    if (!warningEnabled && !criticalEnabled)
      return 'Enable at least one of warning / critical threshold.';
    if (warningEnabled && !Number.isFinite(warningValue))
      return 'Warning threshold must be a number.';
    if (criticalEnabled && !Number.isFinite(criticalValue))
      return 'Critical threshold must be a number.';
    if (warningEnabled && criticalEnabled) {
      const w = Number(warningValue);
      const c = Number(criticalValue);
      if (direction === 'above' && c < w)
        return 'When direction is above, critical must be ≥ warning.';
      if (direction === 'below' && c > w)
        return 'When direction is below, critical must be ≤ warning.';
    }
    if (!Number.isFinite(windowSecs) || windowSecs < 30)
      return 'Window must be at least 30 seconds.';
    if (windowSecs > 7 * 86_400) return 'Window must be at most 7 days.';
    return null;
  });

  async function save() {
    err = null;
    if (validationError) {
      err = validationError;
      return;
    }
    busy = true;
    const input: AlertRuleInput = {
      name: name.trim(),
      enabled,
      metric,
      aggregation,
      direction,
      warning_threshold: warningEnabled ? Number(warningValue) : null,
      critical_threshold: criticalEnabled ? Number(criticalValue) : null,
      window_secs: Math.round(windowSecs),
      targets,
      webhook_uuids: webhookUuids
    };
    try {
      if (initial) {
        await api.updateAlertRule(initial.uuid, input);
      } else {
        await api.createAlertRule(input);
      }
      onSaved();
      onClose();
    } catch (e) {
      err = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  const inputStyle =
    'background: var(--bg); border-color: var(--border); color: var(--fg)';
</script>

<Modal title={initial ? 'Edit alert' : 'New alert'} {onClose}>
  <form
    onsubmit={(e) => {
      e.preventDefault();
      void save();
    }}
    class="space-y-4 text-xs"
  >
    <label class="block">
      <span style="color: var(--muted)">Name</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        bind:value={name}
        type="text"
        required
        autofocus
        placeholder="e.g. prod-us latency"
        class="w-full mt-0.5 px-2 py-1 rounded border"
        style={inputStyle}
      />
    </label>

    <label class="flex items-center gap-2">
      <input type="checkbox" bind:checked={enabled} />
      <span>Enabled</span>
    </label>

    <div>
      <span style="color: var(--muted)">Targets</span>
      <div class="relative mt-0.5 rounded border" style="border-color: var(--border)">
        <div class="flex flex-wrap items-center gap-1 px-1.5 py-1 min-h-[28px]">
          {#each targets as t, i (`${t.kind}:${t.uuid}`)}
            <span
              class="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[11px] mono"
              style="background: var(--border); color: var(--fg)"
            >
              {targetLabel(t)}
              <button
                type="button"
                onclick={() => removeTarget(i)}
                class="ml-0.5 text-[10px] opacity-60 hover:opacity-100"
                aria-label="Remove target"
              >
                ✕
              </button>
            </span>
          {/each}
          <input
            bind:this={targetInputEl}
            bind:value={targetSearch}
            type="text"
            onfocus={() => (targetPickerOpen = true)}
            onblur={() => {
              // Defer so a click on a suggestion (which fires mousedown
              // before blur) can still register its onclick.
              setTimeout(() => (targetPickerOpen = false), 120);
            }}
            onkeydown={onTargetInputKeydown}
            placeholder={targets.length === 0
              ? 'Type to add hosts or groups…'
              : 'Add another…'}
            class="flex-1 min-w-[120px] bg-transparent outline-none"
            style="color: var(--fg)"
          />
        </div>
        {#if targetPickerOpen && targetSuggestions.length > 0}
          <div
            bind:this={targetListEl}
            class="absolute left-0 right-0 top-full mt-0.5 z-10 max-h-60 overflow-y-auto rounded border shadow-md"
            style="background: var(--bg); border-color: var(--border)"
            role="listbox"
          >
            {#each targetSuggestions as s, i (`${s.kind}:${s.uuid}`)}
              <button
                type="button"
                onmousedown={(e) => e.preventDefault()}
                onmouseenter={() => (targetHighlighted = i)}
                onclick={() => addTarget(s)}
                class="w-full text-left px-2 py-0.5 mono flex items-center gap-2 leading-tight"
                style:background={i === targetHighlighted
                  ? 'rgba(128, 128, 128, 0.25)'
                  : 'transparent'}
                role="option"
                aria-selected={i === targetHighlighted}
              >
                <span
                  class="inline-block w-4 text-center text-[10px] uppercase shrink-0"
                  style="color: var(--muted)"
                >
                  {s.kind === 'group' ? 'G' : 'H'}
                </span>
                <span class="truncate">{s.label}</span>
              </button>
            {/each}
          </div>
        {/if}
      </div>
      <p class="text-[11px] mt-1" style="color: var(--muted)">
        Each group expands to every host underneath it at evaluation time.
        Hosts and groups can be freely mixed.
      </p>
    </div>

    <div class="grid grid-cols-2 gap-2">
      <label class="block">
        <span style="color: var(--muted)">Metric</span>
        <select bind:value={metric} class="w-full mt-0.5 px-2 py-1 rounded border" style={inputStyle}>
          {#each METRIC_ORDER as m (m)}
            <option value={m}>{METRIC_LABELS[m]}</option>
          {/each}
        </select>
      </label>
      <label class="block">
        <span style="color: var(--muted)">Aggregation</span>
        <select
          bind:value={aggregation}
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style={inputStyle}
        >
          {#each AGG_ORDER as a (a)}
            <option value={a}>{AGG_LABELS[a]}</option>
          {/each}
        </select>
      </label>
    </div>

    <div class="grid grid-cols-2 gap-2">
      <label class="block">
        <span style="color: var(--muted)">Window (seconds)</span>
        <input
          bind:value={windowSecs}
          type="number"
          min="30"
          step="30"
          class="w-full mt-0.5 px-2 py-1 rounded border"
          style={inputStyle}
        />
        <span class="block text-[10px]" style="color: var(--muted)">
          Span of recent samples the aggregation runs over.
        </span>
      </label>
      <div>
        <span style="color: var(--muted)">Direction</span>
        <div class="mt-0.5 flex gap-2 text-xs">
          <label class="flex items-center gap-1">
            <input type="radio" name="dir" value="above" checked={direction === 'above'}
              onchange={() => (direction = 'above')} />
            <span>above</span>
          </label>
          <label class="flex items-center gap-1">
            <input type="radio" name="dir" value="below" checked={direction === 'below'}
              onchange={() => (direction = 'below')} />
            <span>below</span>
          </label>
        </div>
        <span class="block text-[10px] mt-1" style="color: var(--muted)">
          {direction === 'above'
            ? 'Fire when the aggregated value rises above the threshold.'
            : 'Fire when the aggregated value drops below the threshold.'}
        </span>
      </div>
    </div>

    <fieldset class="rounded border p-3 space-y-2" style="border-color: var(--border)">
      <legend class="px-1 text-[10px] uppercase tracking-wider" style="color: var(--muted)">
        Thresholds (at least one)
      </legend>
      <div class="flex items-center gap-2">
        <input id="warn-on" type="checkbox" bind:checked={warningEnabled} />
        <label for="warn-on" class="flex-1" style="color: var(--latency-warn)">Warning at</label>
        <input
          bind:value={warningValue}
          type="number"
          step="any"
          disabled={!warningEnabled}
          class="w-32 px-2 py-0.5 rounded border mono"
          style={inputStyle}
        />
      </div>
      <div class="flex items-center gap-2">
        <input id="crit-on" type="checkbox" bind:checked={criticalEnabled} />
        <label for="crit-on" class="flex-1" style="color: var(--latency-bad)">Critical at</label>
        <input
          bind:value={criticalValue}
          type="number"
          step="any"
          disabled={!criticalEnabled}
          class="w-32 px-2 py-0.5 rounded border mono"
          style={inputStyle}
        />
      </div>
    </fieldset>

    <div>
      <span style="color: var(--muted)">Webhooks</span>
      {#if !webhooksAccessible}
        <p class="text-[11px] mt-0.5" style="color: var(--muted)">
          Only admins can manage the webhook library. Ask an admin to add
          webhooks under Settings → Alert webhooks if you need to wire one
          up.
        </p>
      {:else if webhooks.length === 0}
        <p class="text-[11px] mt-0.5" style="color: var(--muted)">
          No webhooks defined yet. Add some under Settings → Alert webhooks,
          or leave this rule with no notifier (state still tracks).
        </p>
      {:else}
        <div class="mt-0.5 flex flex-col gap-1 rounded border p-2" style="border-color: var(--border)">
          {#each webhooks as w (w.uuid)}
            <label class="flex items-center gap-2 text-xs">
              <input
                type="checkbox"
                checked={webhookUuids.includes(w.uuid)}
                onchange={() => toggleWebhook(w.uuid)}
              />
              <span class="mono">{w.name}</span>
              <span class="text-[10px] truncate" style="color: var(--muted)">{w.url}</span>
            </label>
          {/each}
        </div>
      {/if}
    </div>

    {#if validationError}
      <p class="text-[11px]" style="color: var(--latency-warn)">⚠ {validationError}</p>
    {/if}
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
        type="submit"
        disabled={busy || !!validationError}
        class="px-3 py-1 rounded font-medium"
        style="background: var(--btn-bg); color: var(--btn-text); opacity: {busy || validationError ? 0.6 : 1}"
      >
        {busy ? 'Saving…' : initial ? 'Save changes' : 'Create alert'}
      </button>
    </div>
  </form>
</Modal>
