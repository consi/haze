// Typed fetch wrapper. Hand-rolled until utoipa annotations cover more endpoints
// and `npm run gen-api` becomes useful (lands in phase 5).

import { base } from '$app/paths';
import { clearThrottled, noteThrottled } from './rate-limit.svelte';

export type Role = 'admin' | 'user' | 'reader' | 'disabled';

export interface User {
  id: number;
  username: string;
  role: Role;
}

export interface ProbeType {
  kind: string;
  config_schema: Record<string, unknown> | null;
}

export interface Group {
  /** Opaque identifier - the only thing the API uses to address groups. */
  uuid: string;
  parent_uuid: string | null;
  display_name: string;
  depth: number;
  created_at: number;
  /** Set when this group originated via cross-instance replication. Frontend
   *  renders the row in dark grey and disables edits other than rename. */
  replication_peer_id?: number | null;
}

export interface Host {
  /** Opaque identifier shared with HZC storage. */
  uuid: string;
  /** Group memberships (UUIDs). Empty = root-level host. */
  group_uuids: string[];
  display_name: string;
  probe_type: string;
  /** Typed differently per probe_type; the new-host form builds it. */
  probe_config: Record<string, unknown>;
  interval_secs: number;
  samples_per_period: number;
  /** HZC chunk window this host was created with. Frozen for life - the
   *  global setting can't migrate existing hosts. */
  chunk_window_secs: number;
  enabled: boolean;
  created_at: number;
  /** Set when this host's metadata + samples come from a replication peer.
   *  The frontend renders dark grey and disables probe-config edits when
   *  set; only `display_name` is editable. */
  replication_peer_id?: number | null;
}

/** One tier in the HZC retention policy. `resolution_secs: 0` means raw. */
export interface RetentionTier {
  max_age_secs: number;
  resolution_secs: number;
}

export interface StorageSettings {
  retention_tiers: RetentionTier[];
  /** Seconds between compactor passes. Reloaded live by the backend. */
  compactor_interval_secs: number;
}

/** Per-area concurrency caps. Read/written through `/api/v1/settings/workers`. */
export interface WorkerPools {
  probe_ping: number;
  probe_traceroute: number;
  trace_every_entries: number;
  trace_queue_timeout_secs: number;
  trace_timeout_secs: number;
  trace_reply_timeout_ms: number;

  probe_dns: number;
  probe_tcp_connect: number;
  probe_tls_connect: number;
  probe_http_ttfb: number;
  probe_http_total: number;
  compactor: number;
  alert_eval: number;
  /** Concurrent replication operations (catch-up + per-rule streams). */
  replication: number;
}

export interface WorkerSettings {
  pools: WorkerPools;
}

export interface SeriesPoint {
  ts: number;
  min?: number;
  p2_5?: number;
  p25?: number;
  median?: number;
  p75?: number;
  p97_5?: number;
  loss_pct?: number;
}

// ─── Alerting ───────────────────────────────────────────────────────────────

export type Severity = 'ok' | 'warning' | 'critical';

/** Which Slot field a rule aggregates across the sliding window. */
export type AlertMetric =
  | 'min'
  | 'p2_5'
  | 'p25'
  | 'median'
  | 'p75'
  | 'p97_5'
  | 'loss_pct';

/** Aggregation applied to the chosen metric across every sample in the window. */
export type AlertAggregation =
  | 'max'
  | 'avg'
  | 'min'
  | 'p50'
  | 'p75'
  | 'p90'
  | 'p95'
  | 'p99';

export type AlertDirection = 'above' | 'below';
export type AlertTargetKind = 'host' | 'group';

export interface AlertTarget {
  kind: AlertTargetKind;
  uuid: string;
}

export interface AlertRule {
  uuid: string;
  name: string;
  enabled: boolean;
  metric: AlertMetric;
  aggregation: AlertAggregation;
  direction: AlertDirection;
  warning_threshold: number | null;
  critical_threshold: number | null;
  window_secs: number;
  targets: AlertTarget[];
  webhook_uuids: string[];
  created_at: number;
  updated_at: number;
}

export interface AlertRuleInput {
  name: string;
  enabled: boolean;
  metric: AlertMetric;
  aggregation: AlertAggregation;
  direction: AlertDirection;
  warning_threshold: number | null;
  critical_threshold: number | null;
  window_secs: number;
  targets: AlertTarget[];
  webhook_uuids: string[];
}

export interface AlertState {
  rule_uuid: string;
  host_uuid: string;
  severity: Severity;
  since: number;
  last_notified_at: number | null;
  /** Aggregated value at the last transition. */
  last_value: number | null;
  /** Threshold the value was compared against. */
  last_threshold: number | null;
}

export interface WebhookHeader {
  name: string;
  value: string;
}

export interface Webhook {
  uuid: string;
  name: string;
  url: string;
  /** Optional custom headers sent on every POST. */
  headers: WebhookHeader[];
  created_at: number;
  updated_at: number;
}

export interface WebhookInput {
  name: string;
  url: string;
  headers: WebhookHeader[];
}

export interface AlertingSettings {
  eval_interval_secs: number;
  webhook_timeout_secs: number;
  snapshot_flush_interval_secs: number;
  min_window_secs: number;
  max_window_secs: number;
}

export interface HostDefaults {
  interval_secs: number;
  samples_per_period: number;
}

/** Public-mode toggle + anonymous-traffic rate limits. Read by every
 *  client (the `enabled` flag is also surfaced via `/server-info` so the
 *  layout can branch before login); written by admin from /settings. */
export interface PublicModeSettings {
  enabled: boolean;
  light_per_minute: number;
  light_burst: number;
  series_per_minute: number;
  series_burst: number;
  sse_max_per_ip: number;
}

export interface ServerInfo {
  passkeys_enabled: boolean;
  /** Whether anonymous browsing is enabled on this instance. */
  public_mode_enabled: boolean;
  version: string;
  /** Stable per-process UUID; surfaced next to the Replication section
   *  header so operators know what they're handing to a peer. */
  instance_uuid: string;
}

export interface WebhookTestResult {
  status: number | null;
  detail: string;
}

export interface SeriesResp {
  host_uuid: string;
  resolution_secs: number;
  from: number;
  to: number;
  samples: SeriesPoint[];
}

// Global handler invoked when an API call returns 401 outside the
// expected auth-probe paths. The layout registers this to redirect to
// /login so a session revoked in the background (logout in another tab,
// admin-initiated password reset) doesn't leave the user stranded on an
// authenticated page that silently fails every action.
let onUnauthorized: (() => void) | null = null;
export function setUnauthorizedHandler(h: () => void) {
  onUnauthorized = h;
}

// Paths where a 401 is a normal outcome and must NOT trigger the global
// redirect - otherwise we'd loop on the login page or stomp the initial
// "are you signed in?" probe.
const UNAUTH_OK_PREFIXES = ['/auth/login', '/auth/me', '/auth/passkey/login'];

// Anonymous traffic can trip the public-mode rate limiter. When it does,
// the server returns 429 with a `Retry-After` header. We honour it up to
// this cap, sleep, and retry the request once. The reactive
// `rateLimitState` surfaces the wait so the UI can show a banner.
const RETRY_AFTER_MAX_SECS = 30;
const RETRY_AFTER_FALLBACK_SECS = 2;

function parseRetryAfter(raw: string | null): number {
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return RETRY_AFTER_FALLBACK_SECS;
  return Math.min(n, RETRY_AFTER_MAX_SECS);
}

async function doFetch(method: string, path: string, body?: unknown, init?: RequestInit) {
  const headers: Record<string, string> = (init?.headers as Record<string, string>) ?? {};
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  return fetch(`${base}/api/v1${path}`, {
    method,
    credentials: 'same-origin',
    body: body !== undefined ? JSON.stringify(body) : undefined,
    ...init,
    headers
  });
}

async function req<T>(
  method: string,
  path: string,
  body?: unknown,
  init?: RequestInit
): Promise<T> {
  let res = await doFetch(method, path, body, init);

  if (res.status === 429) {
    const waitSecs = parseRetryAfter(res.headers.get('Retry-After'));
    noteThrottled(waitSecs);
    try {
      await new Promise((r) => setTimeout(r, waitSecs * 1000));
      // One retry - if the server is still over the limit, surface a
      // proper 429 error rather than retrying indefinitely.
      res = await doFetch(method, path, body, init);
    } finally {
      clearThrottled();
    }
  }

  if (!res.ok) {
    let detail = res.statusText;
    try {
      const j = (await res.json()) as { detail?: string };
      if (j.detail) detail = j.detail;
    } catch {
      // ignore
    }
    if (
      res.status === 401 &&
      onUnauthorized &&
      !UNAUTH_OK_PREFIXES.some((p) => path.startsWith(p))
    ) {
      onUnauthorized();
    }
    throw new ApiError(res.status, detail);
  }
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

export interface RouteRecord {
  id:string;host_uuid:string;timestamp:number;kind:string;version:number;sequence:number;
  context:{target:string;hops:{ip:string;dns:string|null}[][]}|null;
  data:{event?:string;started?:number;finished?:number;previous_observed?:number;reached?:boolean;error?:string;loss_pct?:number;previous_loss_pct?:number;hops?:{sent:number;received:number;loss_pct:number;avg_ms:number|null}[]};
}
export interface RouteHistory {records:RouteRecord[];next:string|null;newer:string|null;total:number;support:string;timeline:{timestamp:number;traces:number;changes:number;gaps:number;loss_pct:number}[]}
export interface RouteDetail {selected:RouteRecord;trace:RouteRecord|null;previous:RouteRecord|null}
export const api = {
  routeHistory(uuid:string,from:number,to:number,all:boolean,before?:string|null,signal?:AbortSignal,at?:number,newer=false):Promise<RouteHistory>{
    const q=new URLSearchParams({from:String(from),to:String(to),all:String(all)});if(before)q.set('before',before);if(at!==undefined)q.set('at',String(Math.floor(at)));if(newer)q.set('newer','true');
    return req('GET',`/hosts/${uuid}/route-history?${q}`,undefined,{signal});
  },
  routeDetail(uuid:string,id:string,signal?:AbortSignal):Promise<RouteDetail>{return req('GET',`/hosts/${uuid}/route-history/${id}`,undefined,{signal});},

  // Auth
  login(username: string, password: string): Promise<User> {
    return req('POST', '/auth/login', { username, password });
  },
  logout(): Promise<void> {
    return req('POST', '/auth/logout');
  },
  me(): Promise<User> {
    return req('GET', '/auth/me');
  },

  // Passkeys
  passkeyRegisterBegin(): Promise<{ token: string; challenge: unknown }> {
    return req('POST', '/auth/passkey/register/begin');
  },
  passkeyRegisterFinish(token: string, credential: unknown, label?: string): Promise<void> {
    return req('POST', '/auth/passkey/register/finish', { token, credential, label });
  },
  passkeyLoginBegin(): Promise<{ token: string; challenge: unknown }> {
    // Discoverable credential flow - no username required. The browser
    // presents the available resident passkeys to the user.
    return req('POST', '/auth/passkey/login/begin');
  },
  passkeyLoginFinish(token: string, credential: unknown): Promise<User> {
    return req('POST', '/auth/passkey/login/finish', { token, credential });
  },

  // Self-management (/api/v1/user/*)
  changePassword(current_password: string, new_password: string): Promise<void> {
    return req('POST', '/user/password', { current_password, new_password });
  },
  listMyPasskeys(): Promise<
    Array<{ id: number; label: string | null; created_at: number; last_used_at: number | null }>
  > {
    return req('GET', '/user/passkeys');
  },
  deleteMyPasskey(id: number): Promise<void> {
    return req('DELETE', `/user/passkeys/${id}`);
  },
  listMyTokens(): Promise<
    Array<{
      id: number;
      name: string;
      created_at: number;
      expires_at: number | null;
      last_used_at: number | null;
      replication_only: boolean;
    }>
  > {
    return req('GET', '/user/tokens');
  },
  createMyToken(
    name: string,
    expires_at: number | null = null,
    replication_only = false
  ): Promise<{
    id: number;
    name: string;
    plaintext: string;
    created_at: number;
    expires_at: number | null;
    replication_only: boolean;
  }> {
    return req('POST', '/user/tokens', { name, expires_at, replication_only });
  },
  deleteMyToken(id: number): Promise<void> {
    return req('DELETE', `/user/tokens/${id}`);
  },

  // Probes / server
  probes(): Promise<ProbeType[]> {
    return req('GET', '/probes');
  },
  serverInfo(): Promise<ServerInfo> {
    return req('GET', '/server-info');
  },

  // Tree - combined groups + hosts in a single round-trip. Preferred for
  // the sidebar reload path; the standalone listGroups()/listHosts()
  // endpoints stay for code that only needs one side or wants a filter.
  getTree(): Promise<{ groups: Group[]; hosts: Host[] }> {
    return req('GET', '/tree');
  },

  // Groups
  listGroups(): Promise<Group[]> {
    return req('GET', '/groups');
  },
  createGroup(displayName: string, parentUuid: string | null = null): Promise<Group> {
    return req('POST', '/groups', {
      display_name: displayName,
      parent_uuid: parentUuid
    });
  },
  updateGroup(
    uuid: string,
    patch: { display_name?: string; parent_uuid?: string | null }
  ): Promise<void> {
    return req('PATCH', `/groups/${uuid}`, patch);
  },
  deleteGroup(uuid: string): Promise<void> {
    return req('DELETE', `/groups/${uuid}`);
  },

  // Hosts
  listHosts(opts?: {
    groupUuid?: string;
    subtreeOf?: string;
    ungrouped?: boolean;
  }): Promise<Host[]> {
    const params = new URLSearchParams();
    if (opts?.subtreeOf) params.set('subtree_of', opts.subtreeOf);
    else if (opts?.groupUuid) params.set('group_uuid', opts.groupUuid);
    if (opts?.ungrouped) params.set('ungrouped', 'true');
    const q = params.toString();
    return req('GET', `/hosts${q ? '?' + q : ''}`);
  },
  getGroup(uuid: string): Promise<Group> {
    return req('GET', `/groups/${uuid}`);
  },
  getHost(uuid: string): Promise<Host> {
    return req('GET', `/hosts/${uuid}`);
  },
  createHost(input: {
    /** Empty (or omitted) puts the host at the root with no parent groups. */
    group_uuids?: string[];
    display_name: string;
    probe_type: string;
    probe_config: Record<string, unknown>;
    interval_secs?: number;
    samples_per_period?: number;
    /** HZC chunk window. Frozen at host creation; omit to use the
     *  system default (1 h). */
    chunk_window_secs?: number;
  }): Promise<Host> {
    return req('POST', '/hosts', input);
  },
  updateHost(
    uuid: string,
    patch: { display_name?: string; group_uuids?: string[] }
  ): Promise<Host> {
    return req('PATCH', `/hosts/${uuid}`, patch);
  },
  deleteHost(uuid: string): Promise<void> {
    return req('DELETE', `/hosts/${uuid}`);
  },
  getSeries(
    hostUuid: string,
    fromSecs: number,
    toSecs: number,
    maxSamples: number
  ): Promise<SeriesResp> {
    const params = new URLSearchParams({
      from: String(fromSecs),
      to: String(toSecs),
      max_samples: String(Math.round(maxSamples))
    });
    return req('GET', `/hosts/${hostUuid}/series?${params}`);
  },

  // Settings (admin-only)
  getStorageSettings(): Promise<StorageSettings> {
    return req('GET', '/settings/storage');
  },
  updateStorageSettings(input: StorageSettings): Promise<StorageSettings> {
    return req('PUT', '/settings/storage', input);
  },
  getWorkerSettings(): Promise<WorkerSettings> {
    return req('GET', '/settings/workers');
  },
  updateWorkerSettings(input: WorkerSettings): Promise<WorkerSettings> {
    return req('PUT', '/settings/workers', input);
  },

  // Admin user management
  adminListUsers(): Promise<AdminUser[]> {
    return req('GET', '/admin/users');
  },
  adminCreateUser(input: { username: string; password: string; role: Role }): Promise<AdminUser> {
    return req('POST', '/admin/users', input);
  },
  adminUpdateUserRole(id: number, role: Role): Promise<void> {
    return req('PATCH', `/admin/users/${id}`, { role });
  },
  adminResetPassword(id: number, newPassword: string): Promise<void> {
    return req('POST', `/admin/users/${id}/password`, { new_password: newPassword });
  },
  adminDeleteUser(id: number): Promise<void> {
    return req('DELETE', `/admin/users/${id}`);
  },
  /** Trigger a graceful server exit. Requires a supervisor (systemd /
   *  cargo-watch / etc.) to bring the process back. */
  adminRestart(): Promise<void> {
    return req('POST', '/admin/restart');
  },

  // Alerts
  listAlertRules(): Promise<AlertRule[]> {
    return req('GET', '/alerts/rules');
  },
  getAlertRule(uuid: string): Promise<AlertRule> {
    return req('GET', `/alerts/rules/${uuid}`);
  },
  createAlertRule(input: AlertRuleInput): Promise<AlertRule> {
    return req('POST', '/alerts/rules', input);
  },
  updateAlertRule(uuid: string, input: AlertRuleInput): Promise<AlertRule> {
    return req('PUT', `/alerts/rules/${uuid}`, input);
  },
  deleteAlertRule(uuid: string): Promise<void> {
    return req('DELETE', `/alerts/rules/${uuid}`);
  },
  listAlertStates(): Promise<AlertState[]> {
    return req('GET', '/alerts/states');
  },

  // Webhooks (admin-only)
  listWebhooks(): Promise<Webhook[]> {
    return req('GET', '/alerts/webhooks');
  },
  createWebhook(input: WebhookInput): Promise<Webhook> {
    return req('POST', '/alerts/webhooks', input);
  },
  updateWebhook(uuid: string, input: WebhookInput): Promise<Webhook> {
    return req('PUT', `/alerts/webhooks/${uuid}`, input);
  },
  deleteWebhook(uuid: string): Promise<void> {
    return req('DELETE', `/alerts/webhooks/${uuid}`);
  },
  testWebhook(uuid: string): Promise<WebhookTestResult> {
    return req('POST', `/alerts/webhooks/${uuid}/test`);
  },

  // Alerting + host-default settings
  getAlertingSettings(): Promise<{ settings: AlertingSettings }> {
    return req('GET', '/settings/alerting');
  },
  updateAlertingSettings(
    input: AlertingSettings
  ): Promise<{ settings: AlertingSettings }> {
    return req('PUT', '/settings/alerting', { settings: input });
  },
  getHostDefaults(): Promise<{ defaults: HostDefaults }> {
    return req('GET', '/settings/hosts');
  },
  updateHostDefaults(input: HostDefaults): Promise<{ defaults: HostDefaults }> {
    return req('PUT', '/settings/hosts', { defaults: input });
  },

  // Public mode + anonymous rate limits (admin-only PUT)
  getPublicMode(): Promise<{ settings: PublicModeSettings }> {
    return req('GET', '/settings/public');
  },
  updatePublicMode(
    input: PublicModeSettings
  ): Promise<{ settings: PublicModeSettings }> {
    return req('PUT', '/settings/public', { settings: input });
  },

  // Replication (admin-only)
  listReplicationPeers(limit = 20, offset = 0): Promise<Paginated<ReplicationPeer>> {
    return paginatedReq('GET', `/replication/peers?limit=${limit}&offset=${offset}`);
  },
  createReplicationPeer(input: CreateReplicationPeerInput): Promise<ReplicationPeer> {
    return req('POST', '/replication/peers', input);
  },
  updateReplicationPeer(uuid: string, input: UpdateReplicationPeerInput): Promise<void> {
    return req('PATCH', `/replication/peers/${uuid}`, input);
  },
  deleteReplicationPeer(uuid: string): Promise<void> {
    return req('DELETE', `/replication/peers/${uuid}`);
  },
  testReplicationPeer(uuid: string): Promise<ReplicationPeerTestResult> {
    return req('POST', `/replication/peers/${uuid}/test`);
  },
  replicationPeerGroupsPreview(uuid: string): Promise<GroupPreview[]> {
    return req('GET', `/replication/peers/${uuid}/groups-preview`);
  },
  listReplicationRules(opts?: {
    peer_uuid?: string;
    limit?: number;
    offset?: number;
  }): Promise<Paginated<ReplicationRule>> {
    const params = new URLSearchParams();
    if (opts?.peer_uuid) params.set('peer_uuid', opts.peer_uuid);
    params.set('limit', String(opts?.limit ?? 20));
    params.set('offset', String(opts?.offset ?? 0));
    return paginatedReq('GET', `/replication/rules?${params}`);
  },
  createReplicationRule(input: CreateReplicationRuleInput): Promise<ReplicationRule> {
    return req('POST', '/replication/rules', input);
  },
  toggleReplicationRule(uuid: string, enabled: boolean): Promise<void> {
    return req('PATCH', `/replication/rules/${uuid}`, { enabled });
  },
  deleteReplicationRule(uuid: string): Promise<void> {
    return req('DELETE', `/replication/rules/${uuid}`);
  },
  listReplicationInbound(limit = 20, offset = 0): Promise<Paginated<ReplicationInboundSlot>> {
    return paginatedReq('GET', `/replication/inbound?limit=${limit}&offset=${offset}`);
  },
  deleteReplicationInbound(slotUuid: string): Promise<void> {
    return req('DELETE', `/replication/inbound/${slotUuid}`);
  },
  unblockReplicationInbound(slotUuid: string): Promise<void> {
    return req('POST', `/replication/inbound/${slotUuid}/unblock`);
  }
};

/** Paged response wrapper. Total count comes from the `X-Total-Count` header. */
export interface Paginated<T> {
  items: T[];
  total: number;
}

async function paginatedReq<T>(method: string, path: string): Promise<Paginated<T>> {
  const res = await doFetch(method, path);
  if (!res.ok) {
    let detail = res.statusText;
    try {
      const j = (await res.json()) as { detail?: string };
      if (j.detail) detail = j.detail;
    } catch {
      // ignore
    }
    throw new ApiError(res.status, detail);
  }
  const items = ((await res.json()) as T[]) ?? [];
  const totalHeader = res.headers.get('X-Total-Count');
  const total = totalHeader ? Number(totalHeader) : items.length;
  return { items, total };
}

export interface ReplicationPeer {
  uuid: string;
  name: string;
  base_url: string;
  source_instance_uuid: string | null;
  upstream_chain: string[];
  tls_skip_verify: boolean;
  reconcile_interval_secs: number;
  created_at: number;
  last_contact_at: number | null;
  last_error: string | null;
  /** Source's `CARGO_PKG_VERSION` captured on the last successful
   *  worker handshake. Surfaced by the Settings UI's Status column. */
  source_version: string | null;
  /** Latency of the last `/instance-info` round trip (ms). */
  last_latency_ms: number | null;
}

export interface CreateReplicationPeerInput {
  name: string;
  base_url: string;
  api_token: string;
  tls_skip_verify?: boolean;
  reconcile_interval_secs?: number;
}

export interface UpdateReplicationPeerInput {
  name?: string;
  api_token?: string;
  tls_skip_verify?: boolean;
  reconcile_interval_secs?: number;
}

export interface ReplicationPeerTestResult {
  ok: boolean;
  source_instance_uuid: string | null;
  source_version: string | null;
  latency_ms: number;
  error: string | null;
}

export interface GroupPreview {
  uuid: string;
  parent_uuid: string | null;
  display_name: string;
  depth: number;
}

export interface ReplicationRule {
  uuid: string;
  peer_uuid: string;
  peer_name: string;
  source_group_uuid: string;
  dest_group_uuid: string;
  slot_uuid: string | null;
  enabled: boolean;
  created_at: number;
  /** Max `last_synced_ts` across the rule's hosts. UI derives "lag" =
   *  `now() - latest_ingested_ts` and ticks it every second locally. */
  latest_ingested_ts?: number;
  host_count: number;
  last_error?: string;
}

export interface CreateReplicationRuleInput {
  peer_uuid: string;
  /** null = source root. */
  source_group_uuid?: string | null;
  /** null = local destination root. */
  dest_group_uuid?: string | null;
  enabled?: boolean;
}

export interface ReplicationInboundSlot {
  slot_uuid: string;
  peer_instance_uuid: string;
  peer_label: string;
  source_group_uuid: string;
  replication_path: string[];
  created_at: number;
  last_stream_at: number | null;
  host_count: number;
  /** Set when an admin force-removed this slot. The peer's instance UUID
   *  stays on file so reconnects from the same destination are refused
   *  until an admin unblocks via the Inbound table action. */
  blocked_at?: number | null;
}

export interface AdminUser {
  id: number;
  username: string;
  role: Role;
  has_password: boolean;
  created_at: number;
  disabled_at: number | null;
}
