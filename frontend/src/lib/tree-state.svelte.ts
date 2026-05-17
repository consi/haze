// Shared expand/collapse state for the sidebar tree, plus a `reloadKey`
// counter that the layout watches so any mutation can trigger a fresh
// `listGroups()` + `listHosts()` round-trip.
//
// IMPORTANT: every page that creates, edits, or deletes a group/host MUST
// call `reloadTree()` after a successful mutation so the sidebar reflects
// the change without a manual reload.
//
// The expand-set is persisted to localStorage so the user's collapse
// choices survive a refresh. Stale UUIDs (for groups that have been
// deleted) are inert: the renderer only checks "is this group's UUID in
// the set?" so they're effectively ignored. Newly-created groups appear
// collapsed because their UUID isn't in the saved set yet — host changes
// don't affect the set at all.

const LS_KEY = 'haze.treeExpanded';

function loadInitial(): Set<string> {
  if (typeof localStorage === 'undefined') return new Set();
  try {
    const raw = localStorage.getItem(LS_KEY);
    if (!raw) return new Set();
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) {
      return new Set(parsed.filter((x): x is string => typeof x === 'string'));
    }
  } catch {
    // Bad JSON / private-mode quota / etc. — start fresh.
  }
  return new Set();
}

function persist(set: Set<string>): void {
  if (typeof localStorage === 'undefined') return;
  try {
    localStorage.setItem(LS_KEY, JSON.stringify([...set]));
  } catch {
    // Quota exceeded / private mode — silently ignore. The set still
    // works in-memory for the current session.
  }
}

export const treeState = $state<{ expanded: Set<string>; reloadKey: number }>({
  expanded: loadInitial(),
  reloadKey: 0
});

export function expandAll(uuids: string[]) {
  treeState.expanded = new Set(uuids);
  persist(treeState.expanded);
}

export function collapseAll() {
  treeState.expanded = new Set();
  persist(treeState.expanded);
}

export function toggle(uuid: string) {
  const next = new Set(treeState.expanded);
  if (next.has(uuid)) next.delete(uuid);
  else next.add(uuid);
  treeState.expanded = next;
  persist(treeState.expanded);
}

/** Bump the reload key to make the layout refetch the tree. Call after any
 *  successful mutation that affects the host/group list. */
export function reloadTree() {
  treeState.reloadKey++;
}
