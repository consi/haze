<script lang="ts">
  import { canSeeSettings, auth } from '$lib/auth.svelte';
  import Forbidden from '$lib/components/Forbidden.svelte';
  import {
    api,
    ApiError,
    type AdminUser,
    type AlertingSettings,
    type HostDefaults,
    type RetentionTier,
    type Role,
    type StorageSettings,
    type Webhook,
    type WebhookHeader,
    type WorkerPools,
    type WorkerSettings
  } from '$lib/api';
  import { reloadKeys } from '$lib/events.svelte';
  import { base } from '$app/paths';
  import { onMount } from 'svelte';

  // ─── Storage ──────────────────────────────────────────────────────────────
  let storageLoading = $state(true);
  let storageLoadErr = $state<string | null>(null);
  let storageSaving = $state(false);
  let storageSaveErr = $state<string | null>(null);
  let storageSaveOk = $state(false);

  type EditableTier = { id: number; maxAgeHours: string; resolutionSecs: string };
  let nextTierId = 0;
  let tiers = $state<EditableTier[]>([]);
  let compactorIntervalMinutes = $state('60');

  function applyStorageState(s: StorageSettings) {
    tiers = s.retention_tiers.map((t) => ({
      id: nextTierId++,
      maxAgeHours: (t.max_age_secs / 3600).toString(),
      resolutionSecs: t.resolution_secs.toString()
    }));
    compactorIntervalMinutes = (s.compactor_interval_secs / 60).toString();
  }

  function defaultsAsState(): StorageSettings {
    return {
      retention_tiers: [
        { max_age_secs: 7 * 86400, resolution_secs: 0 },
        { max_age_secs: 30 * 86400, resolution_secs: 300 },
        { max_age_secs: 180 * 86400, resolution_secs: 1800 },
        { max_age_secs: 365 * 86400, resolution_secs: 7200 },
        { max_age_secs: 5 * 365 * 86400, resolution_secs: 86400 }
      ],
      compactor_interval_secs: 3600
    };
  }

  function addTier() {
    const lastAgeHours = tiers.length ? parseFloat(tiers[tiers.length - 1].maxAgeHours) || 24 : 24;
    const lastRes = tiers.length ? parseInt(tiers[tiers.length - 1].resolutionSecs) || 0 : 0;
    tiers = [
      ...tiers,
      {
        id: nextTierId++,
        maxAgeHours: (lastAgeHours * 2).toString(),
        resolutionSecs: Math.max(lastRes, 30).toString()
      }
    ];
  }

  function removeTier(id: number) {
    tiers = tiers.filter((t) => t.id !== id);
  }

  function resetStorageDefaults() {
    applyStorageState(defaultsAsState());
  }

  function formatSecs(secs: number): string {
    if (!isFinite(secs) || secs <= 0) return 'raw';
    if (secs % 86400 === 0) {
      const d = secs / 86400;
      return d === 1 ? '1 day' : `${d} days`;
    }
    if (secs % 3600 === 0) {
      const h = secs / 3600;
      return h === 1 ? '1 hour' : `${h} hours`;
    }
    if (secs % 60 === 0) {
      const m = secs / 60;
      return `${m} min`;
    }
    return `${secs} s`;
  }

  function formatHours(h: number): string {
    if (!isFinite(h) || h <= 0) return '–';
    if (h >= 24 && h % 24 === 0) {
      const days = h / 24;
      if (days >= 365 && days % 365 === 0) {
        const y = days / 365;
        return y === 1 ? '1 year' : `${y} years`;
      }
      return days === 1 ? '1 day' : `${days} days`;
    }
    return h === 1 ? '1 hour' : `${h} hours`;
  }

  const parsedStorage = $derived.by<{
    payload: StorageSettings | null;
    error: string | null;
  }>(() => {
    if (tiers.length === 0)
      return { payload: null, error: 'At least one retention tier is required' };

    const compMin = parseFloat(compactorIntervalMinutes);
    if (!isFinite(compMin) || compMin < 1)
      return { payload: null, error: 'Compactor interval must be at least 1 minute' };
    if (compMin > 1440)
      return { payload: null, error: 'Compactor interval must be at most 1440 minutes (24h)' };
    const compactor_interval_secs = Math.round(compMin * 60);

    const out: RetentionTier[] = [];
    let prevAge = 0;
    let prevRes = 0;
    for (let i = 0; i < tiers.length; i++) {
      const t = tiers[i];
      const ageH = parseFloat(t.maxAgeHours);
      const res = parseInt(t.resolutionSecs);
      if (!isFinite(ageH) || ageH <= 0)
        return { payload: null, error: `Tier ${i + 1}: max age must be a positive number` };
      if (!isFinite(res) || res < 0)
        return { payload: null, error: `Tier ${i + 1}: resolution must be 0 or greater` };
      const ageSecs = Math.round(ageH * 3600);
      if (ageSecs <= prevAge)
        return {
          payload: null,
          error: `Tier ${i + 1}: max age must be greater than tier ${i}'s max age`
        };
      if (res < prevRes)
        return {
          payload: null,
          error: `Tier ${i + 1}: resolution must be ≥ tier ${i}'s resolution`
        };
      out.push({ max_age_secs: ageSecs, resolution_secs: res });
      prevAge = ageSecs;
      prevRes = res;
    }
    return {
      payload: { retention_tiers: out, compactor_interval_secs },
      error: null
    };
  });

  async function saveStorage() {
    if (!parsedStorage.payload) return;
    storageSaving = true;
    storageSaveErr = null;
    storageSaveOk = false;
    try {
      const result = await api.updateStorageSettings(parsedStorage.payload);
      applyStorageState(result);
      storageSaveOk = true;
      setTimeout(() => (storageSaveOk = false), 3000);
    } catch (e) {
      storageSaveErr = e instanceof Error ? e.message : 'Failed to save';
    } finally {
      storageSaving = false;
    }
  }

  // ─── User management ──────────────────────────────────────────────────────
  const ROLES: Role[] = ['admin', 'user', 'reader', 'disabled'];

  let usersLoading = $state(true);
  let usersLoadErr = $state<string | null>(null);
  let users = $state<AdminUser[]>([]);
  // Per-row UI state, keyed by user id, so feedback stays scoped to the row
  // the operator is actually working with.
  let rowBusy = $state<Record<number, boolean>>({});
  let rowErr = $state<Record<number, string | null>>({});
  let rowOk = $state<Record<number, string | null>>({});
  // Inline "reset password" state - null = not open for any row.
  let pwOpenFor = $state<number | null>(null);
  let pwValue = $state('');

  // ─── Create user form ─────────────────────────────────────────────────────
  let newUsername = $state('');
  let newPassword = $state('');
  let newRole = $state<Role>('user');
  let createBusy = $state(false);
  let createErr = $state<string | null>(null);
  let createOk = $state<string | null>(null);

  async function createUser(e: SubmitEvent) {
    e.preventDefault();
    createErr = null;
    createOk = null;
    const username = newUsername.trim();
    if (!username) {
      createErr = 'Username is required';
      return;
    }
    if (newPassword.length < 8) {
      createErr = 'Password must be at least 8 characters';
      return;
    }
    createBusy = true;
    try {
      const u = await api.adminCreateUser({ username, password: newPassword, role: newRole });
      createOk = `Created user "${u.username}" with role ${u.role}.`;
      newUsername = '';
      newPassword = '';
      newRole = 'user';
      await refreshUsers();
      setTimeout(() => (createOk = null), 3000);
    } catch (e) {
      createErr = describeApiError(e);
    } finally {
      createBusy = false;
    }
  }

  async function refreshUsers() {
    try {
      users = await api.adminListUsers();
    } catch (e) {
      usersLoadErr = e instanceof Error ? e.message : String(e);
    }
  }

  function setRowState(id: number, opts: { busy?: boolean; err?: string | null; ok?: string | null }) {
    if (opts.busy !== undefined) rowBusy = { ...rowBusy, [id]: opts.busy };
    if (opts.err !== undefined) rowErr = { ...rowErr, [id]: opts.err };
    if (opts.ok !== undefined) rowOk = { ...rowOk, [id]: opts.ok };
  }

  function describeApiError(e: unknown): string {
    if (e instanceof ApiError) {
      if (e.status === 409) return e.message || 'Would leave no active admins';
      return e.message;
    }
    return e instanceof Error ? e.message : String(e);
  }

  async function changeRole(u: AdminUser, role: Role) {
    if (role === u.role) return;
    setRowState(u.id, { busy: true, err: null, ok: null });
    try {
      await api.adminUpdateUserRole(u.id, role);
      await refreshUsers();
      setRowState(u.id, { ok: `Role set to ${role}` });
      setTimeout(() => setRowState(u.id, { ok: null }), 2500);
    } catch (e) {
      setRowState(u.id, { err: describeApiError(e) });
      // Reload to revert the optimistic <select> change.
      await refreshUsers();
    } finally {
      setRowState(u.id, { busy: false });
    }
  }

  function openReset(id: number) {
    pwOpenFor = id;
    pwValue = '';
    setRowState(id, { err: null, ok: null });
  }

  function cancelReset() {
    pwOpenFor = null;
    pwValue = '';
  }

  async function confirmReset(u: AdminUser) {
    if (pwValue.length < 8) {
      setRowState(u.id, { err: 'Password must be at least 8 characters' });
      return;
    }
    setRowState(u.id, { busy: true, err: null, ok: null });
    try {
      await api.adminResetPassword(u.id, pwValue);
      pwOpenFor = null;
      pwValue = '';
      setRowState(u.id, { ok: 'Password reset; existing sessions revoked' });
      setTimeout(() => setRowState(u.id, { ok: null }), 3000);
    } catch (e) {
      setRowState(u.id, { err: describeApiError(e) });
    } finally {
      setRowState(u.id, { busy: false });
    }
  }

  async function deleteUser(u: AdminUser) {
    if (u.id === auth.user?.id) {
      setRowState(u.id, { err: 'You cannot delete your own account.' });
      return;
    }
    const yes = window.confirm(
      `Delete user "${u.username}"?\n\n` +
        'This removes the user and all their passkeys, sessions, and API tokens. ' +
        'It cannot be undone.'
    );
    if (!yes) return;
    setRowState(u.id, { busy: true, err: null, ok: null });
    try {
      await api.adminDeleteUser(u.id);
      await refreshUsers();
    } catch (e) {
      setRowState(u.id, { err: describeApiError(e), busy: false });
    }
  }

  function fmtTs(ts: number | null | undefined): string {
    if (ts == null) return '-';
    return new Date(ts * 1000).toLocaleString();
  }

  // ─── Webhooks (alerting) ──────────────────────────────────────────────────
  let webhooksLoading = $state(true);
  let webhooksLoadErr = $state<string | null>(null);
  let webhooks = $state<Webhook[]>([]);
  // Inline edit state, keyed by webhook id (-1 = inactive). When set, the
  // table renders an inline form row for that webhook.
  let editingWebhook = $state<string | null>(null);
  let editName = $state('');
  let editUrl = $state('');
  let webhookRowBusy = $state<Record<string, boolean>>({});
  let webhookRowErr = $state<Record<string, string | null>>({});
  let webhookRowOk = $state<Record<string, string | null>>({});

  let newWebhookName = $state('');
  let newWebhookUrl = $state('');
  let newWebhookHeaders = $state<WebhookHeader[]>([]);
  let newWebhookBusy = $state(false);
  let newWebhookErr = $state<string | null>(null);
  let newWebhookOk = $state<string | null>(null);

  // Inline-edit headers state, keyed by webhook uuid. Mirrors the
  // editName/editUrl pattern.
  let editHeaders = $state<WebhookHeader[]>([]);

  function addNewHeader() {
    newWebhookHeaders = [...newWebhookHeaders, { name: '', value: '' }];
  }
  function removeNewHeader(i: number) {
    newWebhookHeaders = newWebhookHeaders.filter((_, ix) => ix !== i);
  }
  function addEditHeader() {
    editHeaders = [...editHeaders, { name: '', value: '' }];
  }
  function removeEditHeader(i: number) {
    editHeaders = editHeaders.filter((_, ix) => ix !== i);
  }

  async function refreshWebhooks() {
    try {
      webhooks = await api.listWebhooks();
    } catch (e) {
      webhooksLoadErr = e instanceof Error ? e.message : String(e);
    }
  }

  function setWebhookRow(uuid: string, opts: { busy?: boolean; err?: string | null; ok?: string | null }) {
    if (opts.busy !== undefined) webhookRowBusy = { ...webhookRowBusy, [uuid]: opts.busy };
    if (opts.err !== undefined) webhookRowErr = { ...webhookRowErr, [uuid]: opts.err };
    if (opts.ok !== undefined) webhookRowOk = { ...webhookRowOk, [uuid]: opts.ok };
  }

  function startEditWebhook(w: Webhook) {
    editingWebhook = w.uuid;
    editName = w.name;
    editUrl = w.url;
    // Clone so edits don't mutate the loaded list before save.
    editHeaders = w.headers.map((h) => ({ name: h.name, value: h.value }));
    setWebhookRow(w.uuid, { err: null, ok: null });
  }

  function cancelEditWebhook() {
    editingWebhook = null;
  }

  async function saveEditWebhook(w: Webhook) {
    const name = editName.trim();
    const url = editUrl.trim();
    if (!name) {
      setWebhookRow(w.uuid, { err: 'Name is required' });
      return;
    }
    if (!(url.startsWith('http://') || url.startsWith('https://'))) {
      setWebhookRow(w.uuid, { err: 'URL must start with http:// or https://' });
      return;
    }
    const cleanedHeaders = editHeaders
      .map((h) => ({ name: h.name.trim(), value: h.value }))
      .filter((h) => h.name.length > 0);
    setWebhookRow(w.uuid, { busy: true, err: null });
    try {
      await api.updateWebhook(w.uuid, { name, url, headers: cleanedHeaders });
      editingWebhook = null;
      await refreshWebhooks();
      setWebhookRow(w.uuid, { ok: 'Saved' });
      setTimeout(() => setWebhookRow(w.uuid, { ok: null }), 2000);
    } catch (e) {
      setWebhookRow(w.uuid, { err: e instanceof Error ? e.message : String(e) });
    } finally {
      setWebhookRow(w.uuid, { busy: false });
    }
  }

  async function testWebhook(w: Webhook) {
    setWebhookRow(w.uuid, { busy: true, err: null, ok: null });
    try {
      const res = await api.testWebhook(w.uuid);
      if (res.status != null && res.status >= 200 && res.status < 300) {
        setWebhookRow(w.uuid, { ok: `Delivered (HTTP ${res.status})` });
      } else if (res.status != null) {
        setWebhookRow(w.uuid, {
          err: `HTTP ${res.status}${res.detail ? `: ${res.detail}` : ''}`
        });
      } else {
        setWebhookRow(w.uuid, { err: res.detail || 'Request failed' });
      }
      setTimeout(() => setWebhookRow(w.uuid, { ok: null }), 4000);
    } catch (e) {
      setWebhookRow(w.uuid, { err: e instanceof Error ? e.message : String(e) });
    } finally {
      setWebhookRow(w.uuid, { busy: false });
    }
  }

  async function deleteWebhook(w: Webhook) {
    if (!window.confirm(`Delete webhook "${w.name}"?\n\nAny alert rule that references it must be detached first.`)) {
      return;
    }
    setWebhookRow(w.uuid, { busy: true, err: null });
    try {
      await api.deleteWebhook(w.uuid);
      await refreshWebhooks();
    } catch (e) {
      let msg = e instanceof Error ? e.message : String(e);
      if (e instanceof ApiError && e.status === 409) {
        msg = `In use by alert rule(s): ${e.message}. Detach those rules first.`;
      }
      setWebhookRow(w.uuid, { err: msg });
    } finally {
      setWebhookRow(w.uuid, { busy: false });
    }
  }

  async function createWebhook(e: SubmitEvent) {
    e.preventDefault();
    newWebhookErr = null;
    newWebhookOk = null;
    const name = newWebhookName.trim();
    const url = newWebhookUrl.trim();
    if (!name) {
      newWebhookErr = 'Name is required';
      return;
    }
    if (!(url.startsWith('http://') || url.startsWith('https://'))) {
      newWebhookErr = 'URL must start with http:// or https://';
      return;
    }
    const cleanedHeaders = newWebhookHeaders
      .map((h) => ({ name: h.name.trim(), value: h.value }))
      .filter((h) => h.name.length > 0);
    newWebhookBusy = true;
    try {
      const w = await api.createWebhook({ name, url, headers: cleanedHeaders });
      newWebhookOk = `Added "${w.name}"`;
      newWebhookName = '';
      newWebhookUrl = '';
      newWebhookHeaders = [];
      await refreshWebhooks();
      setTimeout(() => (newWebhookOk = null), 3000);
    } catch (e) {
      newWebhookErr = e instanceof Error ? e.message : String(e);
    } finally {
      newWebhookBusy = false;
    }
  }

  // ─── Alerting tunables ────────────────────────────────────────────────────
  let alertingLoading = $state(true);
  let alertingLoadErr = $state<string | null>(null);
  let alertingSaving = $state(false);
  let alertingSaveErr = $state<string | null>(null);
  let alertingSaveOk = $state(false);

  const DEFAULT_ALERTING: AlertingSettings = {
    eval_interval_secs: 60,
    webhook_timeout_secs: 10,
    snapshot_flush_interval_secs: 300,
    min_window_secs: 30,
    max_window_secs: 604_800
  };
  let alerting = $state<AlertingSettings>({ ...DEFAULT_ALERTING });

  const alertingError = $derived.by(() => {
    if (!(alerting.eval_interval_secs >= 5 && alerting.eval_interval_secs <= 3600))
      return 'Eval interval must be 5..3600 seconds.';
    if (!(alerting.webhook_timeout_secs >= 1 && alerting.webhook_timeout_secs <= 120))
      return 'Webhook timeout must be 1..120 seconds.';
    if (
      !(
        alerting.snapshot_flush_interval_secs >= 30 &&
        alerting.snapshot_flush_interval_secs <= 86_400
      )
    )
      return 'Snapshot flush interval must be 30..86400 seconds.';
    if (alerting.min_window_secs < 1) return 'Min window must be ≥ 1.';
    if (alerting.max_window_secs <= alerting.min_window_secs)
      return 'Max window must be greater than min window.';
    if (alerting.max_window_secs > 30 * 86_400) return 'Max window must be ≤ 30 days.';
    return null;
  });

  async function saveAlerting() {
    if (alertingError) return;
    alertingSaving = true;
    alertingSaveErr = null;
    alertingSaveOk = false;
    try {
      const result = await api.updateAlertingSettings(alerting);
      alerting = { ...result.settings };
      alertingSaveOk = true;
      setTimeout(() => (alertingSaveOk = false), 3000);
    } catch (e) {
      alertingSaveErr = e instanceof Error ? e.message : 'Failed to save';
    } finally {
      alertingSaving = false;
    }
  }

  function resetAlertingDefaults() {
    alerting = { ...DEFAULT_ALERTING };
  }

  // ─── Host defaults (Other) ────────────────────────────────────────────────
  let hostDefaultsLoading = $state(true);
  let hostDefaultsLoadErr = $state<string | null>(null);
  let hostDefaultsSaving = $state(false);
  let hostDefaultsSaveErr = $state<string | null>(null);
  let hostDefaultsSaveOk = $state(false);

  const DEFAULT_HOST_DEFAULTS: HostDefaults = {
    interval_secs: 60,
    samples_per_period: 20
  };
  let hostDefaults = $state<HostDefaults>({ ...DEFAULT_HOST_DEFAULTS });

  const hostDefaultsError = $derived.by(() => {
    if (!(hostDefaults.interval_secs >= 1 && hostDefaults.interval_secs <= 86_400))
      return 'Interval must be 1..86400 seconds.';
    if (!(hostDefaults.samples_per_period >= 1 && hostDefaults.samples_per_period <= 1000))
      return 'Samples per period must be 1..1000.';
    return null;
  });

  async function saveHostDefaults() {
    if (hostDefaultsError) return;
    hostDefaultsSaving = true;
    hostDefaultsSaveErr = null;
    hostDefaultsSaveOk = false;
    try {
      const result = await api.updateHostDefaults(hostDefaults);
      hostDefaults = { ...result.defaults };
      hostDefaultsSaveOk = true;
      setTimeout(() => (hostDefaultsSaveOk = false), 3000);
    } catch (e) {
      hostDefaultsSaveErr = e instanceof Error ? e.message : 'Failed to save';
    } finally {
      hostDefaultsSaving = false;
    }
  }

  function resetHostDefaults() {
    hostDefaults = { ...DEFAULT_HOST_DEFAULTS };
  }

  // ─── Workers ──────────────────────────────────────────────────────────────
  let workersLoading = $state(true);
  let workersLoadErr = $state<string | null>(null);
  let workersSaving = $state(false);
  let workersSaveErr = $state<string | null>(null);
  let workersSaveOk = $state(false);

  const POOL_FIELDS: { key: keyof WorkerPools; label: string; hint: string }[] = [
    { key: 'probe_ping',        label: 'PING probes',        hint: 'In-flight ICMP echoes. Cheap; raise this first.' },
    { key: 'probe_dns',         label: 'DNS probes',         hint: 'In-flight DNS queries.' },
    { key: 'probe_tcp_connect', label: 'TCP CONNECT probes', hint: 'In-flight TCP handshakes.' },
    { key: 'probe_tls_connect', label: 'TLS CONNECT probes', hint: 'In-flight TCP+TLS handshakes.' },
    { key: 'probe_http_ttfb',   label: 'HTTP TTFB probes',   hint: 'In-flight HTTP requests (time-to-first-byte).' },
    { key: 'probe_http_total',  label: 'HTTP TOTAL probes',  hint: 'In-flight HTTP requests with full body read.' },
    { key: 'compactor',         label: 'Chunk compactor',    hint: 'Parallel host directories the compactor walks.' },
    { key: 'alert_eval',        label: 'Alert evaluator',    hint: 'Parallel (rule, host) checks per evaluation tick.' }
  ];

  const DEFAULT_WORKER_POOLS: WorkerPools = {
    probe_ping: 4096,
    probe_dns: 1024,
    probe_tcp_connect: 1024,
    probe_tls_connect: 512,
    probe_http_ttfb: 512,
    probe_http_total: 512,
    compactor: 8,
    alert_eval: 32
  };

  // Server caps. Mirror `MAX_TOTAL_POOL_BUDGET` and per-field cap in
  // settings_routes.rs so the UI shows the limit error inline instead of
  // round-tripping a 422.
  const PER_POOL_MAX = 16_384;
  const TOTAL_POOL_BUDGET = 32_768;

  let pools = $state<WorkerPools>({ ...DEFAULT_WORKER_POOLS });

  const poolsTotal = $derived(
    POOL_FIELDS.reduce((sum, f) => sum + (Number(pools[f.key]) || 0), 0)
  );

  const workersError = $derived.by(() => {
    for (const f of POOL_FIELDS) {
      const v = Number(pools[f.key]);
      if (!Number.isFinite(v) || v <= 0) return `${f.label}: must be a positive integer`;
      if (v > PER_POOL_MAX) return `${f.label}: max ${PER_POOL_MAX} per pool`;
    }
    if (poolsTotal > TOTAL_POOL_BUDGET) {
      return `Total pool size ${poolsTotal} exceeds budget ${TOTAL_POOL_BUDGET}.`;
    }
    return null;
  });

  function resetWorkerDefaults() {
    pools = { ...DEFAULT_WORKER_POOLS };
  }

  // Restart UX: after a worker-settings save the server has to come back to
  // pick up the new semaphore sizes. We persist, ask the server to exit,
  // poll `/healthz` until something answers, then reload. `restarting`
  // gates a full-screen overlay while we wait.
  let restarting = $state(false);

  async function saveWorkersAndRestart() {
    if (workersError) return;
    workersSaving = true;
    workersSaveErr = null;
    workersSaveOk = false;
    try {
      const result = await api.updateWorkerSettings({ pools });
      pools = { ...result.pools };
    } catch (e) {
      workersSaveErr = e instanceof Error ? e.message : 'Failed to save';
      workersSaving = false;
      return;
    }
    try {
      await api.adminRestart();
    } catch (e) {
      workersSaveErr =
        'Settings saved, but restart failed: ' +
        (e instanceof Error ? e.message : String(e));
      workersSaving = false;
      return;
    }
    workersSaving = false;
    restarting = true;
    await waitForServerBack();
    window.location.reload();
  }

  // Poll /healthz until we get a 200. The exit was scheduled with a 500 ms
  // delay so the initial requests are expected to fail; we just keep
  // retrying every second until the supervisor brings the server back.
  // Gives up after 60 s so a missing supervisor doesn't pin the spinner
  // forever.
  async function waitForServerBack() {
    const deadline = Date.now() + 60_000;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        const res = await fetch(`${base}/healthz`, { cache: 'no-store' });
        if (res.ok) return;
      } catch {
        // Network error - server still down. Keep trying.
      }
    }
  }

  onMount(async () => {
    if (!canSeeSettings()) return;
    try {
      const [storageRes, workersRes, alertingRes, hostDefaultsRes] = await Promise.all([
        api.getStorageSettings().catch((e) => {
          storageLoadErr = e instanceof Error ? e.message : String(e);
          return null;
        }),
        api.getWorkerSettings().catch((e) => {
          workersLoadErr = e instanceof Error ? e.message : String(e);
          return null;
        }),
        api.getAlertingSettings().catch((e) => {
          alertingLoadErr = e instanceof Error ? e.message : String(e);
          return null;
        }),
        api.getHostDefaults().catch((e) => {
          hostDefaultsLoadErr = e instanceof Error ? e.message : String(e);
          return null;
        }),
        refreshUsers(),
        refreshWebhooks()
      ]);
      if (storageRes) applyStorageState(storageRes);
      if (workersRes) pools = { ...workersRes.pools };
      if (alertingRes) alerting = { ...alertingRes.settings };
      if (hostDefaultsRes) hostDefaults = { ...hostDefaultsRes.defaults };
    } finally {
      storageLoading = false;
      workersLoading = false;
      usersLoading = false;
      webhooksLoading = false;
      alertingLoading = false;
      hostDefaultsLoading = false;
    }
  });

  // SSE-driven refresh per domain. Reading the counter inside the effect
  // wires the reactivity; the editor's local "currently being typed" state
  // sits in separate fields (`tiers`, `pools`, etc.) so a refresh while
  // the user is mid-edit only re-pulls the server's view — it doesn't
  // stomp uncommitted edits. We deliberately only refetch users and
  // webhooks here; the settings sub-sections (storage/workers/alerting/
  // host defaults) are write-only forms — a remote save by another admin
  // is rare enough that requiring a manual reload there is acceptable.
  $effect(() => {
    void reloadKeys.users;
    if (canSeeSettings()) void refreshUsers();
  });
  $effect(() => {
    void reloadKeys.webhooks;
    if (canSeeSettings()) void refreshWebhooks();
  });
</script>

{#if !canSeeSettings()}
  <Forbidden what="settings" />
{:else}
  <div class="p-6 max-w-4xl space-y-6">
    <h1 class="text-base font-semibold">Settings</h1>

    <!-- ════════════════════════════════════════════════════════════════════
         User management
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        User management
      </h2>

      <section class="border rounded p-3" style="border-color: var(--border)">
        <p class="text-[11px] mb-3" style="color: var(--muted)">
          Roles: <code>admin</code> (everything),
          <code>user</code> (hosts/groups/alerts but not settings),
          <code>reader</code> (read-only),
          <code>disabled</code> (cannot log in). The last active admin is locked - demoting or
          deleting them is refused. Deleting a user removes their passkeys, sessions, and tokens.
        </p>

        {#if usersLoading}
          <p class="text-xs" style="color: var(--muted)">Loading…</p>
        {:else if usersLoadErr}
          <p class="text-xs" style="color: var(--latency-bad)">{usersLoadErr}</p>
        {:else if users.length === 0}
          <p class="text-xs" style="color: var(--muted)">No users.</p>
        {:else}
          <table class="w-full text-xs mono">
            <thead style="color: var(--muted)">
              <tr class="text-left">
                <th class="py-1 pr-2 font-normal">Username</th>
                <th class="py-1 pr-2 font-normal">Role</th>
                <th class="py-1 pr-2 font-normal">Created</th>
                <th class="py-1 font-normal text-right">Actions</th>
              </tr>
            </thead>
            <tbody>
              {#each users as u (u.id)}
                <tr class="border-t align-top" style="border-color: var(--border)">
                  <td class="py-1 pr-2">
                    {u.username}
                    {#if u.id === auth.user?.id}
                      <span class="text-[10px]" style="color: var(--muted)">(you)</span>
                    {/if}
                  </td>
                  <td class="py-1 pr-2">
                    <select
                      value={u.role}
                      onchange={(e) => changeRole(u, (e.currentTarget as HTMLSelectElement).value as Role)}
                      disabled={rowBusy[u.id]}
                      class="px-1 py-0.5 rounded border mono"
                      style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                    >
                      {#each ROLES as r (r)}
                        <option value={r}>{r}</option>
                      {/each}
                    </select>
                  </td>
                  <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(u.created_at)}</td>
                  <td class="py-1 text-right whitespace-nowrap">
                    <button
                      type="button"
                      onclick={() => openReset(u.id)}
                      disabled={rowBusy[u.id]}
                      class="px-1 py-0.5"
                      style="color: var(--fg)"
                    >
                      reset pw
                    </button>
                    <button
                      type="button"
                      onclick={() => deleteUser(u)}
                      disabled={rowBusy[u.id] || u.id === auth.user?.id}
                      class="px-1 py-0.5 ml-1"
                      style="color: var(--latency-bad); opacity: {u.id === auth.user?.id ? 0.4 : 1}"
                    >
                      delete
                    </button>
                  </td>
                </tr>
                {#if pwOpenFor === u.id}
                  <tr style="border-color: var(--border)">
                    <td colspan="4" class="px-2 py-2">
                      <div class="flex gap-2 items-center text-xs">
                        <span style="color: var(--muted)">New password for {u.username}:</span>
                        <input
                          bind:value={pwValue}
                          type="password"
                          minlength="8"
                          autocomplete="new-password"
                          class="flex-1 px-2 py-1 rounded border mono"
                          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                        />
                        <button
                          type="button"
                          onclick={() => confirmReset(u)}
                          disabled={rowBusy[u.id]}
                          class="px-2 py-1 rounded font-medium"
                          style="background: var(--btn-bg); color: var(--btn-text)"
                        >
                          {rowBusy[u.id] ? 'Resetting…' : 'Set password'}
                        </button>
                        <button
                          type="button"
                          onclick={cancelReset}
                          class="px-2 py-1 rounded border"
                          style="border-color: var(--border)"
                        >
                          Cancel
                        </button>
                      </div>
                    </td>
                  </tr>
                {/if}
                {#if rowErr[u.id] || rowOk[u.id]}
                  <tr>
                    <td colspan="4" class="px-2 pb-1">
                      {#if rowErr[u.id]}
                        <p class="text-[11px]" style="color: var(--latency-bad)">
                          {rowErr[u.id]}
                        </p>
                      {/if}
                      {#if rowOk[u.id]}
                        <p class="text-[11px]" style="color: var(--latency-good)">
                          {rowOk[u.id]}
                        </p>
                      {/if}
                    </td>
                  </tr>
                {/if}
              {/each}
            </tbody>
          </table>
        {/if}

        <hr class="my-3" style="border-color: var(--border)" />

        <h3 class="text-xs font-semibold mb-2">Create user</h3>
        <form onsubmit={createUser} class="flex flex-wrap gap-2 items-end text-xs">
          <label class="block">
            <span class="block" style="color: var(--muted)">Username</span>
            <input
              bind:value={newUsername}
              type="text"
              required
              autocomplete="off"
              class="px-2 py-1 rounded border mono"
              style="background: var(--bg); border-color: var(--border); color: var(--fg)"
            />
          </label>
          <label class="block">
            <span class="block" style="color: var(--muted)">Password</span>
            <input
              bind:value={newPassword}
              type="password"
              required
              minlength="8"
              autocomplete="new-password"
              class="px-2 py-1 rounded border mono"
              style="background: var(--bg); border-color: var(--border); color: var(--fg)"
            />
          </label>
          <label class="block">
            <span class="block" style="color: var(--muted)">Role</span>
            <select
              bind:value={newRole}
              class="px-2 py-1 rounded border mono"
              style="background: var(--bg); border-color: var(--border); color: var(--fg)"
            >
              {#each ROLES as r (r)}
                <option value={r}>{r}</option>
              {/each}
            </select>
          </label>
          <button
            type="submit"
            disabled={createBusy}
            class="px-3 py-1 rounded font-medium"
            style="background: var(--btn-bg); color: var(--btn-text); opacity: {createBusy ? 0.6 : 1}"
          >
            {createBusy ? 'Creating…' : 'Create user'}
          </button>
          {#if createErr}
            <p class="w-full text-[11px]" style="color: var(--latency-bad)">{createErr}</p>
          {/if}
          {#if createOk}
            <p class="w-full text-[11px]" style="color: var(--latency-good)">{createOk}</p>
          {/if}
        </form>
      </section>
    </div>

    <!-- ════════════════════════════════════════════════════════════════════
         Alert webhooks
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        Alert webhooks
      </h2>
      <section class="border rounded p-3" style="border-color: var(--border)">
        <p class="text-[11px] mb-3" style="color: var(--muted)">
          Targets for alert notifications. Each rule on the Alerting page can
          pick any subset of the webhooks below. Haze POSTs a JSON payload to
          the URL on every severity transition. Use <strong>Test</strong> to
          fire a synthetic payload (helpful for confirming the receiver is
          reachable and authorised). Deleting a webhook that's still wired to
          a rule is refused; detach it on the Alerting page first.
        </p>

        {#if webhooksLoading}
          <p class="text-xs" style="color: var(--muted)">Loading…</p>
        {:else if webhooksLoadErr}
          <p class="text-xs" style="color: var(--latency-bad)">{webhooksLoadErr}</p>
        {:else}
          {#if webhooks.length > 0}
            <table class="w-full text-xs mono">
              <thead style="color: var(--muted)">
                <tr class="text-left">
                  <th class="py-1 pr-2 font-normal">Name</th>
                  <th class="py-1 pr-2 font-normal">URL</th>
                  <th class="py-1 pr-2 font-normal">Created</th>
                  <th class="py-1 font-normal text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {#each webhooks as w (w.uuid)}
                  {#if editingWebhook === w.uuid}
                    <tr class="border-t align-top" style="border-color: var(--border)">
                      <td class="py-1 pr-2">
                        <input
                          bind:value={editName}
                          type="text"
                          class="w-full px-2 py-0.5 rounded border mono"
                          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                        />
                      </td>
                      <td class="py-1 pr-2" colspan="2">
                        <input
                          bind:value={editUrl}
                          type="url"
                          class="w-full px-2 py-0.5 rounded border mono"
                          style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                        />
                        <div class="mt-1 space-y-1">
                          <div class="text-[10px] uppercase tracking-wider" style="color: var(--muted)">
                            Headers (optional)
                          </div>
                          {#each editHeaders as h, i (i)}
                            <div class="flex gap-1 items-center">
                              <input
                                bind:value={h.name}
                                type="text"
                                placeholder="Header-Name"
                                class="w-40 px-2 py-0.5 rounded border mono"
                                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                              />
                              <input
                                bind:value={h.value}
                                type="text"
                                placeholder="value"
                                class="flex-1 px-2 py-0.5 rounded border mono"
                                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                              />
                              <button
                                type="button"
                                onclick={() => removeEditHeader(i)}
                                class="px-1 text-[11px]"
                                style="color: var(--latency-bad)"
                                aria-label="Remove header"
                              >
                                ✕
                              </button>
                            </div>
                          {/each}
                          <button
                            type="button"
                            onclick={addEditHeader}
                            class="px-2 py-0.5 rounded border text-[10px]"
                            style="border-color: var(--border)"
                          >
                            + Add header
                          </button>
                        </div>
                      </td>
                      <td class="py-1 text-right whitespace-nowrap">
                        <button
                          type="button"
                          onclick={() => saveEditWebhook(w)}
                          disabled={webhookRowBusy[w.uuid]}
                          class="px-1 py-0.5"
                          style="color: var(--latency-good)"
                        >
                          save
                        </button>
                        <button
                          type="button"
                          onclick={cancelEditWebhook}
                          class="px-1 py-0.5 ml-1"
                          style="color: var(--muted)"
                        >
                          cancel
                        </button>
                      </td>
                    </tr>
                  {:else}
                    <tr class="border-t align-top" style="border-color: var(--border)">
                      <td class="py-1 pr-2">
                        {w.name}
                        {#if w.headers.length > 0}
                          <span
                            class="ml-1 text-[10px]"
                            style="color: var(--muted)"
                            title={w.headers.map((h) => `${h.name}: ${h.value}`).join('\n')}
                          >
                            +{w.headers.length} header{w.headers.length === 1 ? '' : 's'}
                          </span>
                        {/if}
                      </td>
                      <td class="py-1 pr-2 truncate" style="max-width: 320px" title={w.url}>{w.url}</td>
                      <td class="py-1 pr-2" style="color: var(--muted)">{fmtTs(w.created_at)}</td>
                      <td class="py-1 text-right whitespace-nowrap">
                        <button
                          type="button"
                          onclick={() => testWebhook(w)}
                          disabled={webhookRowBusy[w.uuid]}
                          class="px-1 py-0.5"
                          style="color: var(--fg)"
                        >
                          test
                        </button>
                        <button
                          type="button"
                          onclick={() => startEditWebhook(w)}
                          disabled={webhookRowBusy[w.uuid]}
                          class="px-1 py-0.5 ml-1"
                          style="color: var(--fg)"
                        >
                          edit
                        </button>
                        <button
                          type="button"
                          onclick={() => deleteWebhook(w)}
                          disabled={webhookRowBusy[w.uuid]}
                          class="px-1 py-0.5 ml-1"
                          style="color: var(--latency-bad)"
                        >
                          delete
                        </button>
                      </td>
                    </tr>
                  {/if}
                  {#if webhookRowErr[w.uuid] || webhookRowOk[w.uuid]}
                    <tr>
                      <td colspan="4" class="px-2 pb-1">
                        {#if webhookRowErr[w.uuid]}
                          <p class="text-[11px]" style="color: var(--latency-bad)">
                            {webhookRowErr[w.uuid]}
                          </p>
                        {/if}
                        {#if webhookRowOk[w.uuid]}
                          <p class="text-[11px]" style="color: var(--latency-good)">
                            {webhookRowOk[w.uuid]}
                          </p>
                        {/if}
                      </td>
                    </tr>
                  {/if}
                {/each}
              </tbody>
            </table>
          {:else}
            <p class="text-[11px]" style="color: var(--muted)">No webhooks defined yet.</p>
          {/if}

          <hr class="my-3" style="border-color: var(--border)" />

          <h3 class="text-xs font-semibold mb-2">Add webhook</h3>
          <form onsubmit={createWebhook} class="text-xs space-y-2">
            <div class="flex flex-wrap gap-2 items-end">
              <label class="block">
                <span class="block" style="color: var(--muted)">Name</span>
                <input
                  bind:value={newWebhookName}
                  type="text"
                  required
                  placeholder="e.g. slack-eng"
                  class="px-2 py-1 rounded border mono"
                  style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                />
              </label>
              <label class="block flex-1 min-w-[280px]">
                <span class="block" style="color: var(--muted)">URL</span>
                <input
                  bind:value={newWebhookUrl}
                  type="url"
                  required
                  placeholder="https://hooks.example.com/services/…"
                  class="w-full px-2 py-1 rounded border mono"
                  style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                />
              </label>
              <button
                type="submit"
                disabled={newWebhookBusy}
                class="px-3 py-1 rounded font-medium"
                style="background: var(--btn-bg); color: var(--btn-text); opacity: {newWebhookBusy ? 0.6 : 1}"
              >
                {newWebhookBusy ? 'Adding…' : 'Add webhook'}
              </button>
            </div>

            <div class="space-y-1">
              <div class="text-[10px] uppercase tracking-wider" style="color: var(--muted)">
                Headers (optional — e.g. Authorization, X-API-Token)
              </div>
              {#each newWebhookHeaders as h, i (i)}
                <div class="flex gap-1 items-center">
                  <input
                    bind:value={h.name}
                    type="text"
                    placeholder="Header-Name"
                    class="w-40 px-2 py-0.5 rounded border mono"
                    style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                  />
                  <input
                    bind:value={h.value}
                    type="text"
                    placeholder="value"
                    class="flex-1 px-2 py-0.5 rounded border mono"
                    style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                  />
                  <button
                    type="button"
                    onclick={() => removeNewHeader(i)}
                    class="px-1 text-[11px]"
                    style="color: var(--latency-bad)"
                    aria-label="Remove header"
                  >
                    ✕
                  </button>
                </div>
              {/each}
              <button
                type="button"
                onclick={addNewHeader}
                class="px-2 py-0.5 rounded border text-[10px]"
                style="border-color: var(--border)"
              >
                + Add header
              </button>
            </div>

            {#if newWebhookErr}
              <p class="text-[11px]" style="color: var(--latency-bad)">{newWebhookErr}</p>
            {/if}
            {#if newWebhookOk}
              <p class="text-[11px]" style="color: var(--latency-good)">{newWebhookOk}</p>
            {/if}
          </form>
        {/if}
      </section>
    </div>

    <!-- ════════════════════════════════════════════════════════════════════
         Alerting tunables
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        Alerting
      </h2>
      <section class="border rounded p-3" style="border-color: var(--border)">
        <p class="text-[11px] mb-3" style="color: var(--muted)">
          Engine-wide knobs. <strong>Eval interval</strong>, <strong>flush interval</strong>,
          and <strong>webhook timeout</strong> are re-read on the next cycle (no restart
          required). <strong>Min / max window</strong> are enforced when creating or
          updating an alert rule.
        </p>

        {#if alertingLoading}
          <p class="text-xs" style="color: var(--muted)">Loading…</p>
        {:else if alertingLoadErr}
          <p class="text-xs" style="color: var(--latency-bad)">{alertingLoadErr}</p>
        {:else}
          <div class="grid grid-cols-2 gap-3 text-xs">
            <label class="block">
              <span class="block" style="color: var(--muted)">Eval interval (s)</span>
              <input
                bind:value={alerting.eval_interval_secs}
                type="number"
                min="5"
                max="3600"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
              <span class="block text-[10px] mt-0.5" style="color: var(--muted)">
                How often the engine re-evaluates every rule. Default 60.
              </span>
            </label>
            <label class="block">
              <span class="block" style="color: var(--muted)">Webhook HTTP timeout (s)</span>
              <input
                bind:value={alerting.webhook_timeout_secs}
                type="number"
                min="1"
                max="120"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
              <span class="block text-[10px] mt-0.5" style="color: var(--muted)">
                Per-POST reqwest timeout. Default 10.
              </span>
            </label>
            <label class="block">
              <span class="block" style="color: var(--muted)">Snapshot flush interval (s)</span>
              <input
                bind:value={alerting.snapshot_flush_interval_secs}
                type="number"
                min="30"
                max="86400"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
              <span class="block text-[10px] mt-0.5" style="color: var(--muted)">
                How often the in-memory series is checkpointed to disk for warm restart.
                Default 300. ±20% jitter added automatically.
              </span>
            </label>
            <div></div>
            <label class="block">
              <span class="block" style="color: var(--muted)">Min window (s)</span>
              <input
                bind:value={alerting.min_window_secs}
                type="number"
                min="1"
                max="86400"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
              <span class="block text-[10px] mt-0.5" style="color: var(--muted)">
                Smallest allowed `window_secs` on a rule. Default 30.
              </span>
            </label>
            <label class="block">
              <span class="block" style="color: var(--muted)">Max window (s)</span>
              <input
                bind:value={alerting.max_window_secs}
                type="number"
                min="2"
                max={30 * 86400}
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
              <span class="block text-[10px] mt-0.5" style="color: var(--muted)">
                Largest allowed `window_secs`. Default 604 800 (7 days).
              </span>
            </label>
          </div>

          {#if alertingError}
            <p class="mt-2 text-xs" style="color: var(--latency-warn)">⚠ {alertingError}</p>
          {/if}
          {#if alertingSaveErr}
            <p class="mt-2 text-xs" style="color: var(--latency-bad)">{alertingSaveErr}</p>
          {/if}
          {#if alertingSaveOk}
            <p class="mt-2 text-xs" style="color: var(--latency-good)">Saved.</p>
          {/if}

          <div class="flex gap-2 mt-3 justify-end items-center text-xs">
            <button
              type="button"
              onclick={resetAlertingDefaults}
              class="px-2 py-1 rounded border"
              style="border-color: var(--border)"
            >
              Reset to defaults
            </button>
            <button
              type="button"
              onclick={saveAlerting}
              disabled={alertingSaving || !!alertingError}
              class="px-3 py-1 rounded font-medium"
              style="background: var(--btn-bg); color: var(--btn-text); opacity: {alertingSaving || alertingError ? 0.6 : 1}"
            >
              {alertingSaving ? 'Saving…' : 'Save alerting settings'}
            </button>
          </div>
        {/if}
      </section>
    </div>

    <!-- ════════════════════════════════════════════════════════════════════
         Storage
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        Storage
      </h2>

      {#if storageLoading}
        <p class="text-xs" style="color: var(--muted)">Loading…</p>
      {:else if storageLoadErr}
        <p class="text-xs" style="color: var(--latency-bad)">{storageLoadErr}</p>
      {:else}
        <section class="border rounded p-3" style="border-color: var(--border)">
          <h3 class="text-xs font-semibold mb-1">Retention tiers</h3>
          <p class="text-[11px] mb-3" style="color: var(--muted)">
            Each tier says "data younger than <em>max age</em> should be kept at
            <em>resolution</em> seconds." Tiers run in age order: the compactor down-samples
            chunks once they age past a tier's max age into the resolution of the next tier,
            and anything past the last tier is deleted. Use <code>0</code> in the resolution
            column to keep raw samples. Rows must be monotonically increasing in both max age
            and resolution. <strong>Changes apply live on the next compactor pass</strong>
            (see schedule below). No restart needed.
          </p>

          <div class="rounded border overflow-hidden mb-2" style="border-color: var(--border)">
            <table class="w-full text-xs mono">
              <thead style="color: var(--muted)">
                <tr class="text-left">
                  <th class="py-1 px-2 font-normal w-10">#</th>
                  <th class="py-1 px-2 font-normal">Max age (hours)</th>
                  <th class="py-1 px-2 font-normal">Resolution (seconds)</th>
                  <th class="py-1 px-2 font-normal w-20 text-right"></th>
                </tr>
              </thead>
              <tbody>
                {#each tiers as tier, i (tier.id)}
                  {@const ageHours = parseFloat(tier.maxAgeHours)}
                  {@const resSecs = parseInt(tier.resolutionSecs)}
                  <tr class="border-t" style="border-color: var(--border)">
                    <td class="py-1 px-2" style="color: var(--muted)">{i + 1}</td>
                    <td class="py-1 px-2">
                      <input
                        bind:value={tier.maxAgeHours}
                        type="number"
                        min="0"
                        step="1"
                        class="w-full px-2 py-0.5 rounded border mono"
                        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                      />
                      <span class="text-[10px]" style="color: var(--muted)">
                        = {formatHours(ageHours)}
                      </span>
                    </td>
                    <td class="py-1 px-2">
                      <input
                        bind:value={tier.resolutionSecs}
                        type="number"
                        min="0"
                        step="1"
                        class="w-full px-2 py-0.5 rounded border mono"
                        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                      />
                      <span class="text-[10px]" style="color: var(--muted)">
                        = {formatSecs(resSecs)}
                      </span>
                    </td>
                    <td class="py-1 px-2 text-right">
                      <button
                        type="button"
                        onclick={() => removeTier(tier.id)}
                        class="px-1 py-0.5"
                        style="color: var(--latency-bad)"
                      >
                        remove
                      </button>
                    </td>
                  </tr>
                {/each}
                {#if tiers.length === 0}
                  <tr>
                    <td colspan="4" class="py-3 px-2 text-center" style="color: var(--muted)">
                      No tiers - all data will be deleted on the next compactor pass.
                    </td>
                  </tr>
                {/if}
              </tbody>
            </table>
          </div>

          <div class="flex gap-2 items-center text-xs">
            <button
              type="button"
              onclick={addTier}
              class="px-2 py-1 rounded border"
              style="border-color: var(--border)"
            >
              + Add tier
            </button>
          </div>
        </section>

        <section class="border rounded p-3" style="border-color: var(--border)">
          <h3 class="text-xs font-semibold mb-1">Compactor schedule</h3>
          <p class="text-[11px] mb-2" style="color: var(--muted)">
            How often the compactor walks every host and applies the retention
            policy above. Changes are picked up within a few seconds: the next
            run fires as soon as the new interval has elapsed since the
            previous one (so shortening the schedule while a long wait is
            already in flight doesn't get stuck on the old sleep).
            Default 60 minutes.
          </p>
          <label class="block text-xs max-w-xs">
            <span style="color: var(--muted)">Run every (minutes)</span>
            <input
              bind:value={compactorIntervalMinutes}
              type="number"
              min="1"
              max="1440"
              step="1"
              class="w-full mt-0.5 px-2 py-1 rounded border mono"
              style="background: var(--bg); border-color: var(--border); color: var(--fg)"
            />
          </label>

          {#if parsedStorage.error}
            <p class="mt-2 text-xs" style="color: var(--latency-warn)">⚠ {parsedStorage.error}</p>
          {/if}
          {#if storageSaveErr}
            <p class="mt-2 text-xs" style="color: var(--latency-bad)">{storageSaveErr}</p>
          {/if}
          {#if storageSaveOk}
            <p class="mt-2 text-xs" style="color: var(--latency-good)">Saved.</p>
          {/if}

          <div class="flex gap-2 mt-3 justify-end items-center text-xs">
            <button
              type="button"
              onclick={resetStorageDefaults}
              class="px-2 py-1 rounded border"
              style="border-color: var(--border)"
            >
              Reset to defaults
            </button>
            <button
              type="button"
              onclick={saveStorage}
              disabled={storageSaving || !!parsedStorage.error}
              class="px-3 py-1 rounded font-medium"
              style="background: var(--btn-bg); color: var(--btn-text); opacity: {storageSaving || parsedStorage.error ? 0.6 : 1}"
            >
              {storageSaving ? 'Saving…' : 'Save storage settings'}
            </button>
          </div>
        </section>
      {/if}
    </div>

    <!-- ════════════════════════════════════════════════════════════════════
         Workers
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        Workers
      </h2>

      {#if workersLoading}
        <p class="text-xs" style="color: var(--muted)">Loading…</p>
      {:else if workersLoadErr}
        <p class="text-xs" style="color: var(--latency-bad)">{workersLoadErr}</p>
      {:else}
        <section class="border rounded p-3" style="border-color: var(--border)">
          <p class="text-[11px] mb-3" style="color: var(--muted)">
            Each value is the maximum number of in-flight async operations the
            corresponding subsystem will allow at once. New work past the cap
            waits on a semaphore for a slot to free up.
            Defaults are sized for tens of thousands of hosts on a typical
            4-8 core box. <strong>Changes take effect on the next server restart.</strong>
            Per-pool max is {PER_POOL_MAX}; the sum of every pool must stay below
            {TOTAL_POOL_BUDGET}.
          </p>

          <div class="rounded border overflow-hidden mb-2" style="border-color: var(--border)">
            <table class="w-full text-xs mono">
              <thead style="color: var(--muted)">
                <tr class="text-left">
                  <th class="py-1 px-2 font-normal">Pool</th>
                  <th class="py-1 px-2 font-normal w-32">Concurrent limit</th>
                  <th class="py-1 px-2 font-normal">Description</th>
                </tr>
              </thead>
              <tbody>
                {#each POOL_FIELDS as f (f.key)}
                  <tr class="border-t" style="border-color: var(--border)">
                    <td class="py-1 px-2">{f.label}</td>
                    <td class="py-1 px-2">
                      <input
                        type="number"
                        min="1"
                        max={PER_POOL_MAX}
                        step="1"
                        bind:value={pools[f.key]}
                        class="w-full px-2 py-0.5 rounded border mono"
                        style="background: var(--bg); border-color: var(--border); color: var(--fg)"
                      />
                    </td>
                    <td class="py-1 px-2 text-[11px]" style="color: var(--muted)">
                      {f.hint}
                    </td>
                  </tr>
                {/each}
                <tr class="border-t" style="border-color: var(--border); background: rgba(255,255,255,0.02)">
                  <td class="py-1 px-2 font-semibold">Total</td>
                  <td class="py-1 px-2 mono" style="color: {poolsTotal > TOTAL_POOL_BUDGET ? 'var(--latency-bad)' : 'var(--fg)'}">
                    {poolsTotal} / {TOTAL_POOL_BUDGET}
                  </td>
                  <td class="py-1 px-2 text-[11px]" style="color: var(--muted)">
                    Hard cap on concurrent workers - keeps runtime perf predictable under load.
                  </td>
                </tr>
              </tbody>
            </table>
          </div>

          {#if workersError}
            <p class="mt-2 text-xs" style="color: var(--latency-warn)">⚠ {workersError}</p>
          {/if}
          {#if workersSaveErr}
            <p class="mt-2 text-xs" style="color: var(--latency-bad)">{workersSaveErr}</p>
          {/if}
          {#if workersSaveOk}
            <p class="mt-2 text-xs" style="color: var(--latency-good)">Saved.</p>
          {/if}

          <div class="flex gap-2 mt-3 justify-end items-center text-xs">
            <button
              type="button"
              onclick={resetWorkerDefaults}
              class="px-2 py-1 rounded border"
              style="border-color: var(--border)"
            >
              Reset to defaults
            </button>
            <button
              type="button"
              onclick={saveWorkersAndRestart}
              disabled={workersSaving || restarting || !!workersError}
              class="px-3 py-1 rounded font-medium"
              style="background: var(--latency-bad); color: #fff; opacity: {workersSaving || restarting || workersError ? 0.6 : 1}"
              title="Persist the new pool sizes and exit the process. A supervisor (systemd / cargo-watch / etc.) brings it back."
            >
              {workersSaving ? 'Saving…' : 'Save worker settings and restart'}
            </button>
          </div>
        </section>
      {/if}
    </div>

    <!-- ════════════════════════════════════════════════════════════════════
         Other
         ════════════════════════════════════════════════════════════════════ -->
    <div class="space-y-2">
      <h2 class="text-sm font-semibold uppercase tracking-wide" style="color: var(--muted)">
        Other
      </h2>
      <section class="border rounded p-3" style="border-color: var(--border)">
        <p class="text-[11px] mb-3" style="color: var(--muted)">
          Values pre-filled into the "Create host" form.
        </p>

        {#if hostDefaultsLoading}
          <p class="text-xs" style="color: var(--muted)">Loading…</p>
        {:else if hostDefaultsLoadErr}
          <p class="text-xs" style="color: var(--latency-bad)">{hostDefaultsLoadErr}</p>
        {:else}
          <div class="grid grid-cols-2 gap-3 text-xs">
            <label class="block">
              <span class="block" style="color: var(--muted)">Interval (s)</span>
              <input
                bind:value={hostDefaults.interval_secs}
                type="number"
                min="1"
                max="86400"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
            </label>
            <label class="block">
              <span class="block" style="color: var(--muted)">Samples per period</span>
              <input
                bind:value={hostDefaults.samples_per_period}
                type="number"
                min="1"
                max="1000"
                step="1"
                class="w-full px-2 py-1 rounded border mono"
                style="background: var(--bg); border-color: var(--border); color: var(--fg)"
              />
            </label>
          </div>

          {#if hostDefaultsError}
            <p class="mt-2 text-xs" style="color: var(--latency-warn)">⚠ {hostDefaultsError}</p>
          {/if}
          {#if hostDefaultsSaveErr}
            <p class="mt-2 text-xs" style="color: var(--latency-bad)">{hostDefaultsSaveErr}</p>
          {/if}
          {#if hostDefaultsSaveOk}
            <p class="mt-2 text-xs" style="color: var(--latency-good)">Saved.</p>
          {/if}

          <div class="flex gap-2 mt-3 justify-end items-center text-xs">
            <button
              type="button"
              onclick={resetHostDefaults}
              class="px-2 py-1 rounded border"
              style="border-color: var(--border)"
            >
              Reset to defaults
            </button>
            <button
              type="button"
              onclick={saveHostDefaults}
              disabled={hostDefaultsSaving || !!hostDefaultsError}
              class="px-3 py-1 rounded font-medium"
              style="background: var(--btn-bg); color: var(--btn-text); opacity: {hostDefaultsSaving || hostDefaultsError ? 0.6 : 1}"
            >
              {hostDefaultsSaving ? 'Saving…' : 'Save host defaults'}
            </button>
          </div>
        {/if}
      </section>
    </div>
  </div>

  {#if restarting}
    <div
      class="fixed inset-0 z-50 flex items-center justify-center"
      style="background: rgba(0,0,0,0.6); color: var(--fg)"
    >
      <div
        class="rounded border px-6 py-4 text-center"
        style="background: var(--bg); border-color: var(--border); min-width: 280px"
      >
        <p class="text-sm font-semibold mb-1">Restarting server…</p>
        <p class="text-xs" style="color: var(--muted)">
          Waiting for the supervisor to bring it back. The page will reload automatically.
        </p>
      </div>
    </div>
  {/if}
{/if}
