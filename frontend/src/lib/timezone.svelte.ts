// User-facing timezone preference for every date/time rendered in the UI.
//
// '' (empty string) = use the browser's local time zone - the default.
// Anything else is an IANA zone name ('UTC', 'America/New_York', ...).
//
// Persisted in localStorage, per-browser. Not synced to the backend; the
// preference is about how a viewer wants to read timestamps, not about
// what time the data was captured in.

import { browser } from '$app/environment';
import { auth } from '$lib/auth.svelte';

const STORAGE_KEY = 'haze.tz';

function loadInitial(): string {
  if (!browser) return '';
  try {
    return localStorage.getItem(STORAGE_KEY) ?? '';
  } catch {
    return '';
  }
}

export const tzPref = $state({ value: loadInitial() });

export function setTimezone(tz: string) {
  tzPref.value = tz;
  if (!browser) return;
  try {
    if (tz === '') localStorage.removeItem(STORAGE_KEY);
    else localStorage.setItem(STORAGE_KEY, tz);
  } catch {
    // localStorage can throw in private mode / quota; preference still
    // applies for this session, just not across reloads.
  }
}

// Pass to Intl.DateTimeFormat's `timeZone` option. Reading this in a Svelte
// reactive scope (`$derived`, `$effect`) wires it up to re-run on change.
//
// Anonymous viewers always get browser-local time, ignoring any stored
// preference - a public-mode visitor landing on a browser where a previous
// admin had picked, say, Asia/Tokyo shouldn't be silently subjected to
// that choice. They get the natural default for their machine.
export function currentTimeZone(): string | undefined {
  if (!auth.user) return undefined;
  return tzPref.value || undefined;
}

/** Browser's detected local zone, e.g. 'Europe/Warsaw'. Used for the
 *  "browser default" label in the picker. */
export function browserTimeZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return 'UTC';
  }
}

/** Picker options: browser default first, then UTC, then every IANA zone
 *  alphabetically. Falls back to a small hand-picked list on (very old)
 *  engines without `Intl.supportedValuesOf`. */
export function listTimezones(): { value: string; label: string }[] {
  const all = allIanaZones();
  const rest = all.filter((z) => z !== 'UTC').sort();
  return [
    { value: '', label: `Browser default (${browserTimeZone()})` },
    { value: 'UTC', label: 'UTC' },
    ...rest.map((z) => ({ value: z, label: z }))
  ];
}

function allIanaZones(): string[] {
  const I = Intl as unknown as { supportedValuesOf?: (k: string) => string[] };
  if (typeof I.supportedValuesOf === 'function') {
    try {
      return I.supportedValuesOf('timeZone');
    } catch {
      // fall through to fallback
    }
  }
  return [
    'UTC',
    'Africa/Cairo',
    'Africa/Johannesburg',
    'America/Chicago',
    'America/Denver',
    'America/Los_Angeles',
    'America/New_York',
    'America/Sao_Paulo',
    'Asia/Dubai',
    'Asia/Kolkata',
    'Asia/Shanghai',
    'Asia/Singapore',
    'Asia/Tokyo',
    'Australia/Sydney',
    'Europe/Berlin',
    'Europe/London',
    'Europe/Moscow',
    'Europe/Paris',
    'Europe/Warsaw',
    'Pacific/Auckland'
  ];
}

// ─── Formatting helpers ────────────────────────────────────────────────────
//
// Each call creates a fresh `Intl.DateTimeFormat` - caching is possible but
// premature: even a heavy chart redraws maybe a few dozen labels, and the
// allocation overhead is invisible next to canvas drawing. The benefit of
// not caching: every formatter automatically picks up the latest tz.

export function fmt(epochSecs: number, opts: Intl.DateTimeFormatOptions): string {
  return new Intl.DateTimeFormat(undefined, {
    ...opts,
    timeZone: currentTimeZone()
  }).format(new Date(epochSecs * 1000));
}

/** "Mon DD, YYYY HH:MM:SS"-style (locale-dependent layout). */
export function fmtFullStamp(epochSecs: number): string {
  return fmt(epochSecs, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false
  });
}

/** Locale's default "date + time" formatting (same shape as
 *  `Date.toLocaleString()` but in the configured zone). */
export function fmtDateTime(epochSecs: number): string {
  return fmt(epochSecs, { dateStyle: 'medium', timeStyle: 'medium' });
}

/** Clock parts of `epochSecs` *in the configured zone*. Used where the
 *  existing code reaches for `d.getHours()` / `getMinutes()` etc. - those
 *  always read browser-local, so we bypass them via `formatToParts`. */
export function partsInZone(epochSecs: number): {
  year: number;
  month: number;
  day: number;
  hour: number;
  minute: number;
  second: number;
} {
  const parts = new Intl.DateTimeFormat('en-US', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
    timeZone: currentTimeZone()
  }).formatToParts(new Date(epochSecs * 1000));
  const get = (t: string) => parseInt(parts.find((p) => p.type === t)?.value ?? '0', 10);
  // en-US occasionally emits '24' for the hour at midnight in some zones;
  // normalise so callers can rely on 0..23.
  return {
    year: get('year'),
    month: get('month'),
    day: get('day'),
    hour: get('hour') % 24,
    minute: get('minute'),
    second: get('second')
  };
}
