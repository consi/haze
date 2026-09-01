export type GraphPageKind = 'host' | 'group';

export type StoredGraphRange =
  | { version: 1; mode: 'live'; preset: string }
  | {
      version: 1;
      mode: 'fixed';
      fromSecs: number;
      toSecs: number;
      preset: string | null;
    };

const STORAGE_PREFIX = 'haze.graphRange';
// Deliberately in-memory: survives SvelteKit client-side navigation, but not a
// full page reload. That lets a host-to-host jump carry the visible window
// while reload still restores the destination page's own session entry.
let navigationRange: StoredGraphRange | null = null;

function key(kind: GraphPageKind, uuid: string): string {
  return `${STORAGE_PREFIX}.${kind}.${uuid}`;
}

function validTimestamp(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

export function loadGraphRange(
  kind: GraphPageKind,
  uuid: string,
  validPresets: ReadonlySet<string>
): StoredGraphRange | null {
  if (navigationRange) return navigationRange;
  if (typeof sessionStorage === 'undefined') return null;
  try {
    const raw = sessionStorage.getItem(key(kind, uuid));
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<StoredGraphRange>;
    if (parsed.version !== 1) return null;
    if (parsed.mode === 'live') {
      if (typeof parsed.preset !== 'string' || !validPresets.has(parsed.preset)) return null;
      navigationRange = { version: 1, mode: 'live', preset: parsed.preset };
      return navigationRange;
    }
    if (
      parsed.mode === 'fixed' &&
      validTimestamp(parsed.fromSecs) &&
      validTimestamp(parsed.toSecs) &&
      parsed.toSecs > parsed.fromSecs &&
      (parsed.preset == null ||
        (typeof parsed.preset === 'string' && validPresets.has(parsed.preset)))
    ) {
      navigationRange = {
        version: 1,
        mode: 'fixed',
        fromSecs: parsed.fromSecs,
        toSecs: parsed.toSecs,
        preset: parsed.preset ?? null
      };
      return navigationRange;
    }
  } catch {
    // Bad JSON or unavailable storage: use the page default.
  }
  return null;
}

export function saveGraphRange(
  kind: GraphPageKind,
  uuid: string,
  range: StoredGraphRange
): void {
  navigationRange = range;
  if (typeof sessionStorage === 'undefined') return;
  try {
    sessionStorage.setItem(key(kind, uuid), JSON.stringify(range));
  } catch {
    // Private mode/quota failures should not break graph interaction.
  }
}
