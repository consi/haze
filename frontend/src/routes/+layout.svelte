<script lang="ts">
  import '../app.css';
  import { onMount } from 'svelte';
  import {
    auth,
    refresh,
    logout,
    canSeeAlerts,
    canSeeSettings,
    canEditGroups,
    canEditHosts
  } from '$lib/auth.svelte';
  import { api, setUnauthorizedHandler, type Group, type Host } from '$lib/api';
  import {
    connectEvents,
    disconnectEvents,
    setEventsUnauthorizedHandler
  } from '$lib/events.svelte';
  import Tree from '$lib/components/Tree.svelte';
  import CreateGroupModal from '$lib/components/CreateGroupModal.svelte';
  import CreateHostModal from '$lib/components/CreateHostModal.svelte';
  import EditGroupModal from '$lib/components/EditGroupModal.svelte';
  import EditHostModal from '$lib/components/EditHostModal.svelte';
  import { collapseAll, expandAll, treeState } from '$lib/tree-state.svelte';
  // expandAll is still used by the "++" header button.
  import { goto } from '$app/navigation';
  import { base } from '$app/paths';
  import { page } from '$app/state';

  // Single redirect path for any code path that detects the session is
  // gone — both the central api.ts 401 trap and the EventSource terminal
  // error route through this. Guard against re-entrant calls (already on
  // /login) so we don't queue infinite navigations.
  function handleUnauthorized() {
    disconnectEvents();
    auth.user = null;
    if (page.url.pathname !== `${base}/login`) void goto(`${base}/login`);
  }
  setUnauthorizedHandler(handleUnauthorized);
  setEventsUnauthorizedHandler(handleUnauthorized);

  let { children } = $props();

  let groups = $state<Group[]>([]);
  let hosts = $state<Host[]>([]);
  let treeLoading = $state(false);

  // Search input (live) + debounced version that drives the tree filter.
  // 150 ms is enough to avoid recomputing on every keystroke without feeling
  // sluggish; filtering is O(n hosts) substring match, plenty fast for our
  // host counts.
  let searchInput = $state('');
  let searchDebounced = $state('');
  $effect(() => {
    const v = searchInput;
    const timer = setTimeout(() => {
      searchDebounced = v;
    }, 150);
    return () => clearTimeout(timer);
  });

  async function loadTree() {
    if (!auth.user) return;
    treeLoading = true;
    try {
      // Initial expand-state is loaded from localStorage by tree-state.svelte;
      // we don't seed it here. Groups whose UUID isn't in the saved set
      // render collapsed, so brand-new installs start fully collapsed and
      // new groups appear collapsed by default. Host adds/removes have no
      // effect on the set.
      const tree = await api.getTree();
      groups = tree.groups;
      hosts = tree.hosts;
    } finally {
      treeLoading = false;
    }
  }

  onMount(async () => {
    await refresh();
    if (auth.user) await loadTree();
    else if (page.url.pathname !== `${base}/login`) void goto(`${base}/login`);
  });

  // Re-fetch the tree whenever `auth.user` changes or another page calls
  // `reloadTree()` (which bumps `treeState.reloadKey`). The effect tracks the
  // reloadKey by reading it inside the dependency block so Svelte 5 wires
  // the reactivity automatically.
  $effect(() => {
    const _key = treeState.reloadKey;
    void _key;
    if (auth.user) void loadTree();
  });

  // Open/close the SSE stream as the auth state flips. Both functions are
  // idempotent, so calling them on every transition is safe. Logging in
  // from /login sets auth.user → connect; logout / 401 redirect clears it
  // → disconnect.
  $effect(() => {
    if (auth.user) connectEvents();
    else disconnectEvents();
  });

  async function doLogout() {
    disconnectEvents();
    await logout();
    void goto(`${base}/login`);
  }

  // ─── Resizable tree pane ─────────────────────────────────────────────────
  // Width is persisted to localStorage so it survives reloads. The handle is
  // a 4 px-wide strip between <aside> and <main>; while dragging we listen on
  // window so the pointer can travel anywhere on screen, and we only commit
  // to localStorage on pointerup to avoid hammering it on every frame.
  const TREE_WIDTH_KEY = 'haze.treeWidth';
  const TREE_MIN = 160;
  const TREE_MAX = 640;
  const TREE_DEFAULT = 240;

  let treeWidth = $state(TREE_DEFAULT);
  let dragging = $state(false);

  // Modal state for the sidebar +G / +H buttons. Both open inline overlays
  // rather than navigating to a separate page, so the user keeps their tree
  // context (selected host, scroll position, expansion state) intact.
  let groupModalOpen = $state(false);
  let hostModalOpen = $state(false);
  // Edit modals are open when these hold a target. The Tree fires
  // `onEditGroup` / `onEditHost`, the layout snapshots the group/host,
  // and the modal renders against that snapshot. Modal save closes by
  // setting back to null and triggers a tree reload.
  let editGroupTarget = $state<import('$lib/api').Group | null>(null);
  let editHostTarget = $state<import('$lib/api').Host | null>(null);

  onMount(() => {
    if (typeof localStorage === 'undefined') return;
    const raw = localStorage.getItem(TREE_WIDTH_KEY);
    if (raw == null) return;
    const v = parseInt(raw, 10);
    if (Number.isFinite(v)) treeWidth = clamp(v, TREE_MIN, TREE_MAX);
  });

  function clamp(v: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, v));
  }

  function startResize(e: PointerEvent) {
    e.preventDefault();
    dragging = true;
    const startX = e.clientX;
    const startW = treeWidth;

    const onMove = (ev: PointerEvent) => {
      treeWidth = clamp(startW + (ev.clientX - startX), TREE_MIN, TREE_MAX);
    };
    const onUp = () => {
      dragging = false;
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', onUp);
      try {
        localStorage.setItem(TREE_WIDTH_KEY, String(Math.round(treeWidth)));
      } catch {
        // localStorage can throw in private mode / when full - ignore.
      }
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', onUp);
  }
</script>

<div class="min-h-screen flex flex-col">
  <header class="border-b flex items-baseline gap-3 px-3 py-1.5" style="border-color: var(--border)">
    <a
      href={`${base}/`}
      class="font-display"
      style="color: var(--fg); font-weight: 700; font-size: 16px; line-height: 1"
    >
      Haze
    </a>
    <span class="font-display" style="color: var(--muted); font-weight: 400; font-size: 10px">
      network latency monitor
    </span>
    <span class="ml-auto text-xs flex items-center gap-3" style="color: var(--muted)">
      {#if auth.user}
        <a
          href={`${base}/user`}
          class="mono hover:text-[var(--fg)]"
          style="color: var(--fg); font-weight: {auth.user.role === 'admin' ? 600 : 400}"
        >
          {auth.user.username}
        </a>
        {#if canSeeAlerts()}
          <a href={`${base}/alerting`} class="hover:text-[var(--fg)]">alerting</a>
        {/if}
        {#if canSeeSettings()}
          <a href={`${base}/settings`} class="hover:text-[var(--fg)]">settings</a>
        {/if}
        <button onclick={doLogout} class="hover:text-[var(--fg)]">log out</button>
      {/if}
    </span>
  </header>

  {#if page.url.pathname === `${base}/login` || !auth.user}
    <main class="flex-1">
      {@render children?.()}
    </main>
  {:else}
    <div class="flex-1 flex" style:user-select={dragging ? 'none' : null} style:cursor={dragging ? 'col-resize' : null}>
      <aside
        class="overflow-y-auto shrink-0"
        style="width: {treeWidth}px; border-color: var(--border)"
      >
        <div class="px-2 py-1.5 flex items-center gap-2">
          <input
            bind:value={searchInput}
            type="search"
            placeholder="Search..."
            class="flex-1 text-xs px-1.5 py-0.5 rounded border outline-none"
            style="background: var(--bg); border-color: var(--border); color: var(--fg)"
          />
          <span class="flex gap-1 text-xs font-semibold">
            {#if canEditGroups()}
              <button
                type="button"
                title="Add Group"
                onclick={() => (groupModalOpen = true)}
                class="w-5 h-5 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >G</button>
            {/if}
            {#if canEditHosts()}
              <button
                type="button"
                title="Add Host"
                onclick={() => (hostModalOpen = true)}
                class="w-5 h-5 flex items-center justify-center rounded leading-none"
                style="background: var(--btn-bg); color: #fff"
              >H</button>
            {/if}
            <button
              type="button"
              title="Expand All"
              onclick={() => expandAll(groups.map((g) => g.uuid))}
              class="w-5 h-5 flex items-center justify-center rounded leading-none"
              style="background: var(--btn-bg); color: #fff"
            >+</button>
            <button
              type="button"
              title="Collapse All"
              onclick={() => collapseAll()}
              class="w-5 h-5 flex items-center justify-center rounded leading-none"
              style="background: var(--btn-bg); color: #fff"
            >−</button>
          </span>
        </div>
        {#if !treeLoading && groups.length === 0 && hosts.length === 0}
          <p class="text-[11px] px-2 py-2 italic" style="color: var(--muted); opacity: 0.7">
            Nothing defined yet.
          </p>
        {:else}
          <Tree
            {groups}
            {hosts}
            search={searchDebounced}
            onEditGroup={(g) => (editGroupTarget = g)}
            onEditHost={(h) => (editHostTarget = h)}
          />
        {/if}
      </aside>
      <!-- Resize handle: 4 px hit zone, 1 px visible divider centred on it.
           role="separator" lets screen readers announce it as resizable. -->
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize tree pane"
        tabindex="-1"
        onpointerdown={startResize}
        class="shrink-0 cursor-col-resize"
        style="width: 4px; background: var(--border); opacity: {dragging ? 0.6 : 0.25}; transition: opacity 100ms"
        onmouseenter={(e) => (e.currentTarget.style.opacity = '0.6')}
        onmouseleave={(e) => {
          if (!dragging) e.currentTarget.style.opacity = '0.25';
        }}
      ></div>
      <main class="flex-1 overflow-y-auto min-w-0">
        {@render children?.()}
      </main>
    </div>
  {/if}
</div>

{#if groupModalOpen}
  <CreateGroupModal onClose={() => (groupModalOpen = false)} />
{/if}
{#if hostModalOpen}
  <CreateHostModal onClose={() => (hostModalOpen = false)} />
{/if}
{#if editGroupTarget}
  <EditGroupModal
    group={editGroupTarget}
    allGroups={groups}
    onClose={() => (editGroupTarget = null)}
  />
{/if}
{#if editHostTarget}
  <EditHostModal
    host={editHostTarget}
    allGroups={groups}
    onClose={() => (editHostTarget = null)}
  />
{/if}
