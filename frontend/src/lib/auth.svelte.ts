// Auth state (Svelte 5 runes). Caches `me` so layouts can render without a
// per-page round trip.

import { api, ApiError, type User } from './api';

export const auth = $state<{
  user: User | null;
  loading: boolean;
  error: string | null;
}>({
  user: null,
  loading: true,
  error: null
});

// Whether the server allows anonymous browsing of the read-only UI. Set
// from `/server-info` early in the layout's onMount, refreshed when the
// `Settings` SSE event fires (admin toggling the flag). When true and
// `auth.user` is null, the layout renders the trimmed public variant
// instead of redirecting to /login.
//
// `initialized` distinguishes "we haven't asked the server yet" from "we
// asked and the answer was no". Any code that decides to redirect
// unauthenticated traffic to /login MUST gate on `initialized`, otherwise
// the first render (default `enabled = false`) bounces the visitor before
// the /server-info fetch resolves.
export const publicMode = $state<{ enabled: boolean; initialized: boolean }>({
  enabled: false,
  initialized: false
});

export function setPublicMode(enabled: boolean) {
  publicMode.enabled = enabled;
  publicMode.initialized = true;
}

// Role-gating helpers - mirror the four roles defined in
// crates/haze-auth::user::Role (admin / user / reader / disabled).
// Always check via these helpers rather than comparing role strings inline,
// so the gating stays consistent if a role is renamed or added.

export function isAdmin(): boolean {
  return auth.user?.role === 'admin';
}
export function canSeeSettings(): boolean {
  return auth.user?.role === 'admin';
}
export function canSeeAlerts(): boolean {
  return (
    publicMode.enabled || auth.user?.role === 'admin' || auth.user?.role === 'user'
  );
}
export function canEditAlerts(): boolean {
  return auth.user?.role === 'admin' || auth.user?.role === 'user';
}
export function canEditHosts(): boolean {
  return auth.user?.role === 'admin' || auth.user?.role === 'user';
}
export function canEditGroups(): boolean {
  return auth.user?.role === 'admin' || auth.user?.role === 'user';
}

export async function refresh(): Promise<void> {
  auth.loading = true;
  auth.error = null;
  try {
    auth.user = await api.me();
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) {
      auth.user = null;
    } else {
      auth.error = String(e);
    }
  } finally {
    auth.loading = false;
  }
}

export async function login(username: string, password: string): Promise<void> {
  auth.user = await api.login(username, password);
}

export async function logout(): Promise<void> {
  await api.logout();
  auth.user = null;
}
