// Reactive viewport-size flag for components that need JS-level mobile
// branching (drawer open/close behaviour, touch-only event wiring). CSS
// stays the source of truth for purely visual responsive switches via
// Tailwind's `md:` utilities; this rune covers the cases where JS needs
// to know the same answer.
//
// Threshold matches Tailwind's `md` breakpoint (768px), so a viewport at
// or below 767 reports as mobile and at or above 768 reports as desktop.

const QUERY = '(max-width: 767px)';

function initialMatch(): boolean {
  if (typeof window === 'undefined') return false;
  return window.matchMedia(QUERY).matches;
}

export const viewport = $state({ isMobile: initialMatch() });

let mql: MediaQueryList | null = null;
let listener: ((e: MediaQueryListEvent) => void) | null = null;

/**
 * Start the matchMedia listener. Safe to call from a component's onMount;
 * idempotent - repeated calls reuse the existing subscription.
 */
export function startViewportTracking(): () => void {
  if (typeof window === 'undefined') return () => {};
  if (mql) return () => {};
  mql = window.matchMedia(QUERY);
  listener = (e) => {
    viewport.isMobile = e.matches;
  };
  // Sync once in case the SSR/early-mount initial value lost an edge.
  viewport.isMobile = mql.matches;
  mql.addEventListener('change', listener);
  return () => {
    if (mql && listener) mql.removeEventListener('change', listener);
    mql = null;
    listener = null;
  };
}
