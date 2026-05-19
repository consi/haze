// Server-Sent Events client. The backend at `/api/v1/events` pushes a
// `change` event whenever a domain entity is mutated; we route each kind
// to a reactive counter so any open page that depends on that domain
// re-fetches via its own `$effect` block.
//
// Lifecycle: `connectEvents()` is idempotent - call it whenever the user
// becomes authenticated. `disconnectEvents()` closes the stream (logout).
//
// Session revocation: EventSource closes (readyState → CLOSED) on any
// non-2xx response, so a session that's gone server-side surfaces as a
// terminal error on either the initial connect or the next auto-reconnect.
// We route that to the unauthorized handler so the tab redirects to /login
// even while it's idle, without waiting for the user's next API call.

import { base } from '$app/paths';

import { clearThrottled, noteThrottled } from './rate-limit.svelte';
import { treeState } from './tree-state.svelte';

export const reloadKeys = $state({
  tree: 0,
  alerts: 0,
  webhooks: 0,
  users: 0,
  settings: 0
});

let es: EventSource | null = null;
let intentionalClose = false;
let unauthorizedHandler: (() => void) | null = null;
// Pending reconnect after a 429. Cancelled by disconnectEvents() so we
// don't leave a stale timer pointing at a closed session.
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

export function setEventsUnauthorizedHandler(h: () => void) {
  unauthorizedHandler = h;
}

function bumpAll() {
  treeState.reloadKey++;
  reloadKeys.tree++;
  reloadKeys.alerts++;
  reloadKeys.webhooks++;
  reloadKeys.users++;
  reloadKeys.settings++;
}

export function connectEvents() {
  if (es) return;
  intentionalClose = false;
  if (reconnectTimer != null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  const source = new EventSource(`${base}/api/v1/events`);
  // Listen for the named `change` events the backend emits. Default
  // unnamed messages are not used.
  source.addEventListener('change', (ev) => {
    const data = (ev as MessageEvent<string>).data;
    switch (data) {
      case 'tree':
        // Bump the existing tree reload key so the layout's pre-existing
        // effect (watching treeState.reloadKey) refetches without any
        // change to the modal save-then-reloadTree callers.
        treeState.reloadKey++;
        reloadKeys.tree++;
        break;
      case 'alerts':
        reloadKeys.alerts++;
        break;
      case 'webhooks':
        reloadKeys.webhooks++;
        break;
      case 'users':
        reloadKeys.users++;
        break;
      case 'settings':
        reloadKeys.settings++;
        break;
      case 'all':
        // Server's "I lagged you, refetch everything" signal.
        bumpAll();
        break;
      default:
        // Unknown kind - be lenient, refetch everything rather than miss a
        // forward-compatible push from a newer server.
        bumpAll();
        break;
    }
  });
  source.addEventListener('error', () => {
    if (intentionalClose) return;
    // CLOSED is terminal - EventSource only enters it on non-2xx responses
    // (including 401 when the session has been revoked, or 429 when an
    // anonymous IP hits the per-IP SSE cap) or when we asked it to close.
    // Transient network blips leave readyState in CONNECTING, so we don't
    // act on those.
    if (source.readyState === EventSource.CLOSED) {
      es = null;
      void handleTerminalClose();
    }
  });
  es = source;
}

// EventSource hides the HTTP status code, so to distinguish "session
// revoked" (401 - log out) from "anonymous IP cap hit" (429 - wait and
// reconnect) we probe the same URL with a plain fetch. The probe is one
// extra request per disconnect and only fires on terminal close; the cost
// is acceptable given the alternative is either spamming the limiter via
// EventSource's blind auto-reconnect, or logging out on every transient
// rate-limit blip.
async function handleTerminalClose() {
  if (intentionalClose) return;
  let status: number | null = null;
  let retryAfter = 5;
  try {
    const res = await fetch(`${base}/api/v1/events`, {
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { Accept: 'text/event-stream' }
    });
    status = res.status;
    const ra = res.headers.get('Retry-After');
    const n = Number(ra);
    if (Number.isFinite(n) && n > 0) retryAfter = Math.min(n, 60);
    // Drain so we don't leak the response body.
    try {
      await res.body?.cancel();
    } catch {
      // ignore
    }
  } catch {
    // Network down - treat as unauthorized so the user lands on /login
    // rather than a half-broken authenticated page.
  }
  if (status === 429) {
    noteThrottled(retryAfter);
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      clearThrottled();
      if (!intentionalClose) connectEvents();
    }, retryAfter * 1000);
    return;
  }
  // 401, unknown 4xx/5xx, or network failure: fall through to the
  // existing redirect-to-login flow.
  unauthorizedHandler?.();
}

export function disconnectEvents() {
  if (reconnectTimer != null) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  if (!es) return;
  intentionalClose = true;
  es.close();
  es = null;
}
