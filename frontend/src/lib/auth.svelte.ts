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
  return auth.user?.role === 'admin' || auth.user?.role === 'user';
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
